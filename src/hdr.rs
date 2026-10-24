//! HDR output and colour-managed surfaces, on the GLES renderer.
//!
//! On an HDR monitor everything is first composited into a half-float buffer holding what an SDR
//! frame would (gamma 2.2, BT.709, white = 1.0), except that HDR content may go above 1.0 and
//! wide-gamut colours below 0. Ordinary windows and our own UI draw into it unchanged. A final
//! pass turns it into BT.2100 PQ for the monitor, placing SDR white at `sdr_brightness` nits and
//! rolling highlights off towards the panel's peak.
//!
//! Surfaces whose app described their colours (wp_color_management_v1) are drawn through a
//! conversion shader into that same encoding, on SDR and HDR monitors alike; on an SDR monitor
//! their highlights are compressed to fit.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, Offscreen, Texture,
            damage::OutputDamageTracker,
            element::{Element, Id, Kind, RenderElement, RenderElementStates, UnderlyingStorage},
            gles::{
                GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType,
                UniformValue,
            },
            utils::{CommitCounter, DamageBag, DamageSet, OpaqueRegions},
        },
    },
    output::Output,
    utils::{Buffer, Physical, Rectangle, Scale, Size, Transform},
};

use crate::{
    color::{BT709, BT2020, ConvertParams, DisplayColor, HdrOutputParams, column_major, rgb_to_rgb},
    config::Color,
    render::{FrameElement, OutputElement},
};

/// Shader pieces both programs share: precision and the PQ curve.
const COMMON: &str = r#"
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif

const float PQ_M1 = 0.1593017578125;
const float PQ_M2 = 78.84375;
const float PQ_C1 = 0.8359375;
const float PQ_C2 = 18.8515625;
const float PQ_C3 = 18.6875;

vec3 pq_eotf(vec3 v) {
    vec3 p = pow(clamp(v, 0.0, 1.0), vec3(1.0 / PQ_M2));
    return pow(max(p - PQ_C1, 0.0) / (PQ_C2 - PQ_C3 * p), vec3(1.0 / PQ_M1));
}

vec3 pq_inv_eotf(vec3 l) {
    vec3 p = pow(clamp(l, 0.0, 1.0), vec3(PQ_M1));
    return pow((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p), vec3(PQ_M2));
}

vec3 spow(vec3 v, float e) {
    return sign(v) * pow(abs(v), vec3(e));
}
"#;

const HEADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif
"#;

const SAMPLER: &str = r#"
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
varying vec2 v_coords;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
"#;

/// A described surface to the composited encoding (see `ConvertParams`).
const CONVERT: &str = r#"
uniform int tf;
uniform float tf_exp;
uniform float in_scale;
uniform mat3 to_709;
uniform float peak;
uniform int hdr_target;

float srgb_eotf(float v) {
    return v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4);
}

vec3 eotf(vec3 c) {
    if (tf == 0) {
        c = clamp(c, 0.0, 1.0);
        return vec3(srgb_eotf(c.r), srgb_eotf(c.g), srgb_eotf(c.b));
    } else if (tf == 1) {
        vec3 a = abs(c);
        return sign(c) * vec3(srgb_eotf(a.r), srgb_eotf(a.g), srgb_eotf(a.b));
    } else if (tf == 2) {
        return spow(c, tf_exp);
    } else if (tf == 3) {
        return pq_eotf(c);
    }
    return c;
}

void main() {
    vec4 color = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    color.a = 1.0;
#endif
    vec3 rgb = color.a > 0.0 ? color.rgb / color.a : vec3(0.0);
    rgb = to_709 * (eotf(rgb) * in_scale);
    if (peak > 0.0) {
        // HDR content on an SDR target: below 75% of white unchanged, brighter rolls off
        // towards white, keeping the hue.
        float m = max(rgb.r, max(rgb.g, rgb.b));
        if (m > 0.75) {
            float mapped = 0.75 + 0.25 * (1.0 - exp(-(m - 0.75) / 0.25));
            rgb *= mapped / m;
        }
    }
    if (hdr_target == 0) {
        rgb = clamp(rgb, 0.0, 1.0);
    }
    rgb = spow(rgb, 1.0 / 2.2);
    color = vec4(rgb * color.a, color.a) * alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif
    gl_FragColor = color;
}
"#;

/// The composited HDR frame to BT.2100 PQ (see `color::encode_pq_reference`).
const ENCODE: &str = r#"
uniform float sdr_white;
uniform float peak_nits;
uniform mat3 to_2020;

vec3 tonemap(vec3 rgb) {
    float m = max(rgb.r, max(rgb.g, rgb.b));
    float knee = 0.75 * peak_nits;
    if (m <= knee) {
        return rgb;
    }
    float room = peak_nits - knee;
    float mapped = knee + room * (1.0 - exp(-(m - knee) / room));
    return rgb * (mapped / m);
}

void main() {
    vec3 rgb = spow(texture2D(tex, v_coords).rgb, 2.2);
    rgb = max(to_2020 * rgb, 0.0) * sdr_white;
    gl_FragColor = vec4(pq_inv_eotf(tonemap(rgb) / 10000.0), 1.0) * alpha;
}
"#;

#[derive(Clone)]
struct Shaders {
    convert: GlesTexProgram,
    encode: GlesTexProgram,
}

/// Compiled once per renderer and kept with it. None if the driver can't compile them.
fn shaders(renderer: &mut GlesRenderer) -> Option<Shaders> {
    let data = renderer.egl_context().user_data();
    if let Some(s) = data.get::<Option<Shaders>>() {
        return s.clone();
    }
    let u = |name: &'static str, type_| UniformName {
        name: name.into(),
        type_,
    };
    let convert = renderer.compile_custom_texture_shader(
        format!("{HEADER}{COMMON}{SAMPLER}{CONVERT}"),
        &[
            u("tf", UniformType::_1i),
            u("tf_exp", UniformType::_1f),
            u("in_scale", UniformType::_1f),
            u("to_709", UniformType::Matrix3x3),
            u("peak", UniformType::_1f),
            u("hdr_target", UniformType::_1i),
        ],
    );
    let encode = renderer.compile_custom_texture_shader(
        format!("{HEADER}{COMMON}{SAMPLER}{ENCODE}"),
        &[
            u("sdr_white", UniformType::_1f),
            u("peak_nits", UniformType::_1f),
            u("to_2020", UniformType::Matrix3x3),
        ],
    );
    let shaders = match (convert, encode) {
        (Ok(convert), Ok(encode)) => Some(Shaders { convert, encode }),
        (c, e) => {
            let err = c.err().or(e.err()).map(|e| e.to_string()).unwrap_or_default();
            tracing::error!("HDR shaders failed to compile, colours won't be converted: {err}");
            None
        }
    };
    renderer.egl_context().user_data().insert_if_missing(|| shaders.clone());
    shaders
}

fn mat_uniform(name: &'static str, m: [f32; 9]) -> Uniform<'static> {
    Uniform::new(
        name,
        UniformValue::Matrix3x3 {
            matrices: vec![m],
            transpose: false,
        },
    )
}

/// A surface drawn through the colour conversion shader.
pub struct ColorElement {
    inner: OutputElement<GlesRenderer>,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
}

impl ColorElement {
    /// `inner` converted with `params`, or as it is when no conversion is needed (or possible).
    pub fn wrap(
        renderer: &mut GlesRenderer,
        inner: OutputElement<GlesRenderer>,
        params: &ConvertParams,
        hdr_target: bool,
    ) -> FrameElement {
        if params.is_identity() {
            return FrameElement::Plain(inner);
        }
        let Some(shaders) = shaders(renderer) else {
            return FrameElement::Plain(inner);
        };
        let uniforms = vec![
            Uniform::new("tf", UniformValue::_1i(params.tf)),
            Uniform::new("tf_exp", UniformValue::_1f(params.tf_exp)),
            Uniform::new("in_scale", UniformValue::_1f(params.in_scale)),
            mat_uniform("to_709", params.mat),
            Uniform::new("peak", UniformValue::_1f(params.peak)),
            Uniform::new("hdr_target", UniformValue::_1i(hdr_target as i32)),
        ];
        FrameElement::Color(Self {
            inner,
            program: shaders.convert,
            uniforms,
        })
    }
}

impl Element for ColorElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }
    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }
    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for ColorElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(self.program.clone(), self.uniforms.clone());
        let result = self.inner.draw(frame, src, dst, damage, opaque_regions);
        frame.clear_tex_program_override();
        result
    }

    // Never put on a plane as-is: the monitor would get the unconverted pixels.
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// An HDR monitor's composited frame, drawn through the PQ encoding shader. The only thing the
/// monitor's own compositor sees.
#[derive(Clone)]
pub struct HdrEncodeElement {
    id: Id,
    texture: GlesTexture,
    size: Size<i32, Physical>,
    damage: smithay::backend::renderer::utils::DamageSnapshot<i32, Physical>,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
}

impl Element for HdrEncodeElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.damage.current_commit()
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size()).to_f64()
    }
    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Rectangle::from_size(self.size)
    }
    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.damage
            .damage_since(commit)
            .unwrap_or_else(|| DamageSet::from_slice(&[self.geometry(scale)]))
    }
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::from_slice(&[self.geometry(scale)])
    }
}

impl RenderElement<GlesRenderer> for HdrEncodeElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            1.0,
            Some(&self.program),
            &self.uniforms,
        )
    }
}

/// Per HDR monitor: the half-float compositing buffer and what's needed to encode it.
pub struct HdrPipeline {
    texture: GlesTexture,
    size: Size<i32, Physical>,
    tracker: OutputDamageTracker,
    damage: DamageBag<i32, Physical>,
    id: Id,
    /// The buffer holds the previous frame, so only what changed is redrawn.
    primed: bool,
    params: HdrOutputParams,
    scale: f64,
}

impl HdrPipeline {
    /// For `output` at its current mode, scale and rotation. The compositing buffer is in the
    /// output's own (unrotated) orientation; the monitor's compositor rotates the encoded frame.
    pub fn new(renderer: &mut GlesRenderer, output: &Output, params: HdrOutputParams) -> Result<Self, String> {
        shaders(renderer).ok_or("the HDR shaders didn't compile")?;
        let mode = output.current_mode().ok_or("output has no mode")?;
        let transform = output.current_transform();
        let size: Size<i32, Physical> = transform.transform_size(mode.size);
        let texture: GlesTexture = renderer
            .create_buffer(Fourcc::Abgr16161616f, (size.w, size.h).into())
            .map_err(|e| format!("half-float buffer: {e}"))?;
        let scale = output.current_scale().fractional_scale();
        Ok(Self {
            texture,
            size,
            tracker: OutputDamageTracker::new(size, scale, Transform::Normal),
            damage: DamageBag::new(4),
            id: Id::new(),
            primed: false,
            params,
            scale,
        })
    }

    /// Whether this pipeline still fits the output (same size, scale and rotation).
    pub fn matches(&self, output: &Output, params: HdrOutputParams) -> bool {
        let Some(mode) = output.current_mode() else {
            return false;
        };
        self.size == output.current_transform().transform_size(mode.size)
            && self.params == params
            && self.scale == output.current_scale().fractional_scale()
    }

    /// Composite `elements` into the half-float buffer. Returns the element states (for
    /// presentation feedback) and whether anything changed.
    pub fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        elements: &[FrameElement],
        clear: Color,
    ) -> Result<(RenderElementStates, bool), String> {
        let age = if self.primed { 1 } else { 0 };
        let mut target = renderer.bind(&mut self.texture).map_err(|e| format!("bind: {e}"))?;
        let result = self
            .tracker
            .render_output(renderer, &mut target, age, elements, clear)
            .map_err(|e| format!("render: {e:?}"))?;
        self.primed = true;
        let changed = result.damage.is_some_and(|d| !d.is_empty());
        if let Some(d) = result.damage {
            self.damage.add(d.iter().copied());
        }
        Ok((result.states, changed))
    }

    /// The element that draws the composited frame, PQ-encoded.
    pub fn element(&self, renderer: &mut GlesRenderer) -> Option<HdrEncodeElement> {
        let shaders = shaders(renderer)?;
        Some(HdrEncodeElement {
            id: self.id.clone(),
            texture: self.texture.clone(),
            size: self.size,
            damage: self.damage.snapshot(),
            program: shaders.encode,
            uniforms: vec![
                Uniform::new("sdr_white", UniformValue::_1f(self.params.sdr_white as f32)),
                Uniform::new("peak_nits", UniformValue::_1f(self.params.peak as f32)),
                mat_uniform("to_2020", column_major(&rgb_to_rgb(&BT709, &BT2020))),
            ],
        })
    }
}

/// The kernel's `struct hdr_output_metadata` (HDMI static metadata type 1, PQ), telling the
/// monitor to expect HDR10 and roughly how bright the content gets.
pub fn hdr_output_metadata(display: &DisplayColor) -> [u8; 32] {
    let mut b = [0u8; 32];
    // metadata_type (u32) = HDMI_STATIC_METADATA_TYPE1 (0), then the infoframe at offset 4.
    b[4] = 2; // eotf: SMPTE ST 2084
    b[5] = 0; // metadata_type: static type 1
    let mut put = |off: usize, v: u16| b[off..off + 2].copy_from_slice(&v.to_ne_bytes());
    // Primaries and white point in units of 0.00002, in the order red, green, blue.
    let p = display.primaries.unwrap_or(BT2020);
    let units = |v: f64| (v * 50000.0).round().clamp(0.0, 50000.0) as u16;
    for (i, (x, y)) in [p.r, p.g, p.b].into_iter().enumerate() {
        put(6 + i * 4, units(x));
        put(8 + i * 4, units(y));
    }
    put(18, units(p.w.0));
    put(20, units(p.w.1));
    let peak = display.peak().round().clamp(1.0, 10000.0) as u16;
    put(22, peak); // max_display_mastering_luminance, nits
    let min = (display.min_luminance.unwrap_or(0.005) * 10000.0)
        .round()
        .clamp(0.0, 65535.0) as u16;
    put(24, min); // min_display_mastering_luminance, 0.0001 nits
    put(26, peak); // max_cll
    put(
        28,
        display.max_fall.unwrap_or(display.peak()).round().clamp(1.0, 10000.0) as u16,
    ); // max_fall
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{ImageDesc, Target, Tf, encode_pq_reference};
    use smithay::backend::{
        allocator::gbm::GbmDevice,
        egl::{EGLContext, EGLDisplay},
        renderer::{ExportMem, Frame, Renderer},
    };

    fn renderer() -> Option<GlesRenderer> {
        for n in 128..136 {
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/dev/dri/renderD{n}"))
            else {
                continue;
            };
            let gbm = GbmDevice::new(file).ok()?;
            // SAFETY: test-only; the display and context live for the whole test.
            let display = unsafe { EGLDisplay::new(gbm).ok()? };
            let context = EGLContext::new(&display).ok()?;
            return unsafe { GlesRenderer::new(context).ok() };
        }
        None
    }

    /// Draw a 4×1 texture of `pixels` (f32 RGBA, premultiplied) through `program` into a half-float
    /// target, and read the result back.
    fn run_shader(
        r: &mut GlesRenderer,
        pixels: &[[f32; 4]; 4],
        program: &GlesTexProgram,
        uniforms: &[Uniform<'_>],
    ) -> Vec<[f32; 4]> {
        use smithay::backend::renderer::ImportMem;
        let half: Vec<u8> = pixels
            .iter()
            .flat_map(|p| p.iter().flat_map(|c| half::f16::from_f32(*c).to_ne_bytes()))
            .collect();
        let src = r
            .import_memory(&half, Fourcc::Abgr16161616f, (4, 1).into(), false)
            .expect("import");
        let mut dst: GlesTexture = r.create_buffer(Fourcc::Abgr16161616f, (4, 1).into()).unwrap();
        {
            let mut fb = r.bind(&mut dst).unwrap();
            let mut frame = r.render(&mut fb, (4, 1).into(), Transform::Normal).unwrap();
            frame
                .clear([0.0; 4].into(), &[Rectangle::from_size((4, 1).into())])
                .unwrap();
            frame
                .render_texture_from_to(
                    &src,
                    Rectangle::from_size((4.0, 1.0).into()),
                    Rectangle::from_size((4, 1).into()),
                    &[Rectangle::from_size((4, 1).into())],
                    &[],
                    Transform::Normal,
                    1.0,
                    Some(program),
                    uniforms,
                )
                .unwrap();
            frame.finish().unwrap();
        }
        let fb = r.bind(&mut dst).unwrap();
        let mapping = r
            .copy_framebuffer(&fb, Rectangle::from_size((4, 1).into()), Fourcc::Abgr16161616f)
            .unwrap();
        let bytes = r.map_texture(&mapping).unwrap();
        bytes
            .chunks_exact(8)
            .map(|p| {
                let c = |i: usize| half::f16::from_ne_bytes([p[i], p[i + 1]]).to_f32();
                [c(0), c(2), c(4), c(6)]
            })
            .collect()
    }

    #[test]
    fn encode_shader_matches_reference() {
        let Some(mut r) = renderer() else {
            eprintln!("no GPU, skipped");
            return;
        };
        let shaders = shaders(&mut r).expect("shaders compile");
        let params = HdrOutputParams {
            sdr_white: 203.0,
            peak: 400.0,
        };
        let pixels = [
            [1.0, 1.0, 1.0, 1.0],
            [0.2, 0.4, 0.6, 1.0],
            [2.5, 2.5, 2.5, 1.0],
            [1.0, 0.0, 0.0, 1.0],
        ];
        let uniforms = vec![
            Uniform::new("sdr_white", UniformValue::_1f(203.0)),
            Uniform::new("peak_nits", UniformValue::_1f(400.0)),
            mat_uniform("to_2020", column_major(&rgb_to_rgb(&BT709, &BT2020))),
        ];
        let out = run_shader(&mut r, &pixels, &shaders.encode, &uniforms);
        for (got, px) in out.iter().zip(pixels) {
            let want = encode_pq_reference([px[0] as f64, px[1] as f64, px[2] as f64], params);
            for c in 0..3 {
                assert!(
                    (got[c] as f64 - want[c]).abs() < 2e-3,
                    "{px:?}: got {got:?}, want {want:?}"
                );
            }
        }
    }

    #[test]
    fn convert_shader_takes_pq_content_to_relative_light() {
        let Some(mut r) = renderer() else {
            eprintln!("no GPU, skipped");
            return;
        };
        let shaders = shaders(&mut r).expect("shaders compile");
        // A PQ, BT.2020 surface with reference white at 203 nits, onto the HDR buffer.
        let desc = ImageDesc::hdr(203.0, &DisplayColor::default());
        let p = ConvertParams::new(&desc, Target::HdrIntermediate);
        let uniforms = vec![
            Uniform::new("tf", UniformValue::_1i(p.tf)),
            Uniform::new("tf_exp", UniformValue::_1f(p.tf_exp)),
            Uniform::new("in_scale", UniformValue::_1f(p.in_scale)),
            mat_uniform("to_709", p.mat),
            Uniform::new("peak", UniformValue::_1f(p.peak)),
            Uniform::new("hdr_target", UniformValue::_1i(1)),
        ];
        let pq = |nits: f64| crate::color::pq_inv_eotf(nits / 10000.0) as f32;
        // 203 nits grey is SDR white (1.0); 406 nits is twice as much light; half alpha keeps
        // its premultiplication.
        let w = pq(203.0);
        let d = pq(406.0);
        let pixels = [
            [w, w, w, 1.0],
            [d, d, d, 1.0],
            [w * 0.5, w * 0.5, w * 0.5, 0.5],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let out = run_shader(&mut r, &pixels, &shaders.convert, &uniforms);
        let enc = |l: f64| l.powf(1.0 / 2.2) as f32;
        let e2 = enc(2.0);
        let want = [
            [1.0, 1.0, 1.0, 1.0],
            [e2, e2, e2, 1.0],
            [0.5, 0.5, 0.5, 0.5],
            [0.0, 0.0, 0.0, 1.0],
        ];
        for (got, want) in out.iter().zip(want) {
            for c in 0..4 {
                assert!((got[c] - want[c]).abs() < 4e-3, "got {got:?}, want {want:?}");
            }
        }
        assert_eq!(Tf::Pq, desc.tf);
    }

    #[test]
    fn metadata_blob_layout() {
        let d = DisplayColor {
            max_luminance: Some(400.0),
            min_luminance: Some(0.1),
            ..Default::default()
        };
        let b = hdr_output_metadata(&d);
        assert_eq!(b[4], 2);
        assert_eq!(u16::from_ne_bytes([b[6], b[7]]), 35400); // BT.2020 red x 0.708
        assert_eq!(u16::from_ne_bytes([b[22], b[23]]), 400);
        assert_eq!(u16::from_ne_bytes([b[24], b[25]]), 1000);
    }
}
