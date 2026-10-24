//! Colour spaces for HDR monitors and colour-managed apps: primaries, transfer functions,
//! luminances, and the conversions between them.
//!
//! Everything the compositor draws itself, and every app that doesn't say otherwise, is taken to
//! be sRGB primaries with a gamma 2.2 display curve (what an ordinary monitor shows). On an HDR
//! monitor that content is placed at `sdr_brightness` nits, and HDR content above it.

/// A 3×3 matrix, row-major.
pub type Mat3 = [[f64; 3]; 3];

/// CIE 1931 xy chromaticities of a colour space's red, green and blue primaries and white point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticities {
    pub r: (f64, f64),
    pub g: (f64, f64),
    pub b: (f64, f64),
    pub w: (f64, f64),
}

const D65: (f64, f64) = (0.3127, 0.3290);
const ILLUMINANT_C: (f64, f64) = (0.310, 0.316);

pub const BT709: Chromaticities = Chromaticities {
    r: (0.640, 0.330),
    g: (0.300, 0.600),
    b: (0.150, 0.060),
    w: D65,
};
pub const BT2020: Chromaticities = Chromaticities {
    r: (0.708, 0.292),
    g: (0.170, 0.797),
    b: (0.131, 0.046),
    w: D65,
};

/// The named primaries of wp_color_manager_v1, by their protocol value.
pub fn named_primaries(value: u32) -> Option<Chromaticities> {
    let c = |r, g, b, w| Chromaticities { r, g, b, w };
    Some(match value {
        1 => BT709,
        2 => c((0.67, 0.33), (0.21, 0.71), (0.14, 0.08), ILLUMINANT_C), // PAL-M (BT.470 M)
        3 => c((0.64, 0.33), (0.29, 0.60), (0.15, 0.06), D65),          // PAL (BT.601 625)
        4 => c((0.630, 0.340), (0.310, 0.595), (0.155, 0.070), D65),    // NTSC (BT.601 525)
        5 => c((0.681, 0.319), (0.243, 0.692), (0.145, 0.049), ILLUMINANT_C), // generic film
        6 => BT2020,
        7 => c((1.0, 0.0), (0.0, 1.0), (0.0, 0.0), (1.0 / 3.0, 1.0 / 3.0)), // CIE 1931 XYZ
        8 => c((0.680, 0.320), (0.265, 0.690), (0.150, 0.060), (0.314, 0.351)), // DCI-P3
        9 => c((0.680, 0.320), (0.265, 0.690), (0.150, 0.060), D65),        // Display P3
        10 => c((0.64, 0.33), (0.21, 0.71), (0.15, 0.06), D65),             // Adobe RGB
        _ => return None,
    })
}

/// How signal values turn into light.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tf {
    /// The piecewise sRGB curve (IEC 61966-2-1).
    Srgb,
    /// The same, mirrored for negative values (extended sRGB).
    ExtSrgb,
    /// A pure power curve: gamma 2.2, 2.8, BT.1886 (2.4), or a client's own exponent.
    Power(f64),
    /// SMPTE ST 2084, the perceptual quantizer HDR10 uses.
    Pq,
    /// Linear, extended over all real numbers.
    Linear,
}

impl Tf {
    /// Shader code for the curve, matching the `TF_*` constants in the shader.
    pub fn shader_id(self) -> (i32, f32) {
        match self {
            Tf::Srgb => (0, 0.0),
            Tf::ExtSrgb => (1, 0.0),
            Tf::Power(e) => (2, e as f32),
            Tf::Pq => (3, 0.0),
            Tf::Linear => (4, 0.0),
        }
    }

    /// Signal to normalized light, on the CPU (tests and reference values).
    pub fn eotf(self, v: f64) -> f64 {
        match self {
            Tf::Srgb => srgb_eotf(v.clamp(0.0, 1.0)),
            Tf::ExtSrgb => v.signum() * srgb_eotf(v.abs()),
            Tf::Power(e) => v.signum() * v.abs().powf(e),
            Tf::Pq => pq_eotf(v.clamp(0.0, 1.0)),
            Tf::Linear => v,
        }
    }
}

fn srgb_eotf(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f64 = 3424.0 / 4096.0;
const PQ_C2: f64 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f64 = 2392.0 / 4096.0 * 32.0;

/// PQ signal to light, 1.0 = 10000 nits.
pub fn pq_eotf(v: f64) -> f64 {
    let p = v.max(0.0).powf(1.0 / PQ_M2);
    ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1)
}

/// Light (1.0 = 10000 nits) to PQ signal.
#[cfg(test)]
pub fn pq_inv_eotf(l: f64) -> f64 {
    let p = l.clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2)
}

/// The luminance range a description's signal covers, and its reference (diffuse) white, in nits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Luminances {
    pub min: f64,
    pub max: f64,
    pub reference: f64,
}

impl Luminances {
    /// The defaults a transfer function implies (wp_color_manager_v1).
    pub fn default_for(tf: Tf, bt1886: bool) -> Self {
        match tf {
            Tf::Pq => Self {
                min: 0.005,
                max: 10000.005,
                reference: 203.0,
            },
            _ if bt1886 => Self {
                min: 0.01,
                max: 100.0,
                reference: 100.0,
            },
            _ => Self {
                min: 0.2,
                max: 80.0,
                reference: 80.0,
            },
        }
    }
}

/// Everything known about how a surface's (or an output's) pixels are to be understood.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageDesc {
    pub tf: Tf,
    /// The protocol's name for the transfer function, or None for a power curve.
    pub tf_named: Option<u32>,
    pub primaries: Chromaticities,
    /// The protocol's name for the primaries, if they were given by name.
    pub primaries_named: Option<u32>,
    pub lum: Luminances,
    /// The colours and brightness the content was made for (the mastering display), if known.
    pub target_primaries: Option<Chromaticities>,
    pub target_lum: Option<(f64, f64)>,
    pub max_cll: Option<f64>,
    pub max_fall: Option<f64>,
}

pub const TF_GAMMA22: u32 = 2;
pub const TF_ST2084_PQ: u32 = 11;
pub const PRIMARIES_SRGB: u32 = 1;
pub const PRIMARIES_BT2020: u32 = 6;

impl ImageDesc {
    /// An ordinary monitor, and what apps that say nothing are taken to send.
    pub fn sdr() -> Self {
        Self {
            tf: Tf::Power(2.2),
            tf_named: Some(TF_GAMMA22),
            primaries: BT709,
            primaries_named: Some(PRIMARIES_SRGB),
            lum: Luminances::default_for(Tf::Power(2.2), false),
            target_primaries: None,
            target_lum: None,
            max_cll: None,
            max_fall: None,
        }
    }

    /// A monitor in HDR mode: BT.2100 PQ signalling, with ordinary white at `sdr_white` nits and
    /// the panel's own colours and brightness range as the target.
    pub fn hdr(sdr_white: f64, display: &DisplayColor) -> Self {
        let min = display.min_luminance.unwrap_or(0.005);
        Self {
            tf: Tf::Pq,
            tf_named: Some(TF_ST2084_PQ),
            primaries: BT2020,
            primaries_named: Some(PRIMARIES_BT2020),
            lum: Luminances {
                min,
                max: min + 10000.0,
                reference: sdr_white,
            },
            target_primaries: Some(display.primaries.unwrap_or(BT2020)),
            target_lum: Some((min, display.peak())),
            max_cll: Some(display.peak()),
            max_fall: display.max_fall,
        }
    }

    /// The brightest the content gets, in nits: what the client said, else a typical HDR peak.
    pub fn content_peak(&self) -> f64 {
        self.target_lum
            .map(|(_, max)| max)
            .or(self.max_cll)
            .unwrap_or(if self.tf == Tf::Pq { 1000.0 } else { self.lum.max })
    }
}

/// What a monitor's EDID says about its colours and brightness.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplayColor {
    pub primaries: Option<Chromaticities>,
    /// It understands HDR10 (PQ) signalling.
    pub pq: bool,
    /// It accepts BT.2020 colours.
    pub bt2020: bool,
    pub max_luminance: Option<f64>,
    pub max_fall: Option<f64>,
    pub min_luminance: Option<f64>,
}

impl DisplayColor {
    /// Peak brightness in nits; monitors that don't say are assumed to be HDR400-class.
    pub fn peak(&self) -> f64 {
        self.max_luminance.unwrap_or(400.0)
    }

    /// Read the colour information from an EDID (base block + CTA-861 extensions).
    pub fn from_edid(edid: &[u8]) -> Self {
        let mut out = Self::default();
        if edid.len() < 128 || edid[0..8] != [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0] {
            return out;
        }
        // Chromaticities: bytes 25..35, 10 bits each, low bits packed in bytes 25 and 26.
        let lo1 = edid[25] as u32;
        let lo2 = edid[26] as u32;
        let coord = |hi: usize, lo: u32| ((edid[hi] as u32) << 2 | lo) as f64 / 1024.0;
        let rx = coord(27, (lo1 >> 6) & 3);
        let ry = coord(28, (lo1 >> 4) & 3);
        let gx = coord(29, (lo1 >> 2) & 3);
        let gy = coord(30, lo1 & 3);
        let bx = coord(31, (lo2 >> 6) & 3);
        let by = coord(32, (lo2 >> 4) & 3);
        let wx = coord(33, (lo2 >> 2) & 3);
        let wy = coord(34, lo2 & 3);
        if ry > 0.0 && gy > 0.0 && by > 0.0 && wy > 0.0 {
            out.primaries = Some(Chromaticities {
                r: (rx, ry),
                g: (gx, gy),
                b: (bx, by),
                w: (wx, wy),
            });
        }
        for block in edid.chunks_exact(128).skip(1) {
            // CTA-861 extension: data blocks run from byte 4 to the DTD offset in byte 2.
            if block[0] != 0x02 {
                continue;
            }
            let end = (block[2] as usize).clamp(4, 127);
            let mut i = 4;
            while i < end {
                let tag = block[i] >> 5;
                let len = (block[i] & 0x1f) as usize;
                let data = &block[(i + 1).min(end)..(i + 1 + len).min(end)];
                if tag == 7 && !data.is_empty() {
                    match data[0] {
                        // Colorimetry: bit 7 of the first byte is BT.2020 RGB.
                        5 if data.len() >= 2 => out.bt2020 = data[1] & 0x80 != 0,
                        // HDR static metadata.
                        6 if data.len() >= 3 => {
                            out.pq = data[1] & 0x04 != 0;
                            if let Some(&cv) = data.get(3).filter(|&&v| v > 0) {
                                out.max_luminance = Some(50.0 * 2f64.powf(cv as f64 / 32.0));
                            }
                            if let Some(&cv) = data.get(4).filter(|&&v| v > 0) {
                                out.max_fall = Some(50.0 * 2f64.powf(cv as f64 / 32.0));
                            }
                            if let (Some(max), Some(&cv)) = (out.max_luminance, data.get(5)) {
                                out.min_luminance = Some(max * (cv as f64 / 255.0).powi(2) / 100.0);
                            }
                        }
                        _ => {}
                    }
                }
                i += 1 + len;
            }
        }
        out
    }
}

pub fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut m = [[0.0; 3]; 3];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

pub fn mat_vec(a: &Mat3, v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| a[i][0] * v[0] + a[i][1] * v[1] + a[i][2] * v[2])
}

pub fn mat_inv(m: &Mat3) -> Mat3 {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    let d = 1.0 / det;
    [
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * d,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * d,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * d,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * d,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * d,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * d,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * d,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * d,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * d,
        ],
    ]
}

fn xy_to_xyz((x, y): (f64, f64)) -> [f64; 3] {
    [x / y, 1.0, (1.0 - x - y) / y]
}

/// Linear RGB in these primaries to CIE XYZ (white at Y = 1).
pub fn rgb_to_xyz(c: &Chromaticities) -> Mat3 {
    let [r, g, b] = [c.r, c.g, c.b].map(xy_to_xyz);
    let m = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    let s = mat_vec(&mat_inv(&m), xy_to_xyz(c.w));
    [0, 1, 2].map(|i| [m[i][0] * s[0], m[i][1] * s[1], m[i][2] * s[2]])
}

/// Bradford chromatic adaptation from one white point to another.
fn bradford(from: (f64, f64), to: (f64, f64)) -> Mat3 {
    const B: Mat3 = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let s = mat_vec(&B, xy_to_xyz(from));
    let d = mat_vec(&B, xy_to_xyz(to));
    let scale = [
        [d[0] / s[0], 0.0, 0.0],
        [0.0, d[1] / s[1], 0.0],
        [0.0, 0.0, d[2] / s[2]],
    ];
    mat_mul(&mat_inv(&B), &mat_mul(&scale, &B))
}

/// Linear RGB in `from` primaries to linear RGB in `to` primaries, adapting the white point.
pub fn rgb_to_rgb(from: &Chromaticities, to: &Chromaticities) -> Mat3 {
    let mut m = rgb_to_xyz(from);
    if from.w != to.w {
        m = mat_mul(&bradford(from.w, to.w), &m);
    }
    mat_mul(&mat_inv(&rgb_to_xyz(to)), &m)
}

/// Where a surface is drawn: an ordinary framebuffer (or a screenshot), or the HDR compositing
/// buffer, which holds gamma-2.2-encoded BT.709 relative to SDR white, extended past 0..1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Target {
    Sdr,
    HdrIntermediate,
}

/// The uniforms of the surface conversion shader (see `hdr.rs`).
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertParams {
    pub tf: i32,
    pub tf_exp: f32,
    /// Normalized light to multiples of reference white.
    pub in_scale: f32,
    /// Source primaries to BT.709, column-major for GLSL.
    pub mat: [f32; 9],
    /// The content goes brighter than reference white (its peak, in multiples of it), so on an
    /// SDR target highlights are rolled off to fit. 0 = no roll-off.
    pub peak: f32,
}

impl ConvertParams {
    pub fn new(src: &ImageDesc, target: Target) -> Self {
        let (tf, tf_exp) = src.tf.shader_id();
        let max = if src.tf == Tf::Pq { 10000.0 } else { src.lum.max };
        let in_scale = (max / src.lum.reference) as f32;
        let m = rgb_to_rgb(&src.primaries, &BT709);
        let peak = match target {
            Target::Sdr => {
                let p = src.content_peak() / src.lum.reference;
                if p > 1.001 { p as f32 } else { 0.0 }
            }
            Target::HdrIntermediate => 0.0,
        };
        Self {
            tf,
            tf_exp,
            in_scale,
            mat: column_major(&m),
            peak,
        }
    }

    /// Converting changes nothing: the content is already what the target holds.
    pub fn is_identity(&self) -> bool {
        let id = column_major(&[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        self.tf == 2
            && (self.tf_exp - 2.2).abs() < 1e-4
            && (self.in_scale - 1.0).abs() < 1e-4
            && self.peak == 0.0
            && self.mat.iter().zip(id).all(|(a, b)| (a - b).abs() < 1e-4)
    }
}

pub fn column_major(m: &Mat3) -> [f32; 9] {
    [
        m[0][0], m[1][0], m[2][0], m[0][1], m[1][1], m[2][1], m[0][2], m[1][2], m[2][2],
    ]
    .map(|v| v as f32)
}

/// What the HDR output pass needs: SDR white and the panel's peak, in nits.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrOutputParams {
    pub sdr_white: f64,
    pub peak: f64,
}

/// Reference implementation of the output pass, for tests: an intermediate (gamma-2.2-encoded,
/// BT.709, relative to SDR white) value to the PQ signal sent to the monitor.
#[cfg(test)]
pub fn encode_pq_reference(v: [f64; 3], p: HdrOutputParams) -> [f64; 3] {
    let lin = v.map(|c| c.signum() * c.abs().powf(2.2));
    let bt2020 = mat_vec(&rgb_to_rgb(&BT709, &BT2020), lin);
    let nits = tonemap(bt2020.map(|c| c.max(0.0) * p.sdr_white), p.peak);
    nits.map(|n| pq_inv_eotf(n / 10000.0))
}

/// Squeeze highlights into what the panel can show: up to 75% of its peak passes unchanged,
/// above that brightness rolls off towards the peak.
#[cfg(test)]
pub fn tonemap(rgb: [f64; 3], peak: f64) -> [f64; 3] {
    let m = rgb[0].max(rgb[1]).max(rgb[2]);
    let knee = 0.75 * peak;
    if m <= knee {
        return rgb;
    }
    let room = peak - knee;
    let mapped = knee + room * (1.0 - (-(m - knee) / room).exp());
    rgb.map(|c| c * mapped / m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn bt709_to_xyz_matches_the_standard() {
        let m = rgb_to_xyz(&BT709);
        let want = [
            [0.4124, 0.3576, 0.1805],
            [0.2126, 0.7152, 0.0722],
            [0.0193, 0.1192, 0.9505],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert!(close(m[i][j], want[i][j], 2e-4), "{m:?}");
            }
        }
    }

    #[test]
    fn bt709_to_bt2020_matches_bt2087() {
        let m = rgb_to_rgb(&BT709, &BT2020);
        let want = [
            [0.6274, 0.3293, 0.0433],
            [0.0691, 0.9195, 0.0114],
            [0.0164, 0.0880, 0.8956],
        ];
        for i in 0..3 {
            for j in 0..3 {
                assert!(close(m[i][j], want[i][j], 2e-4), "{m:?}");
            }
        }
        // White stays white.
        let w = mat_vec(&m, [1.0, 1.0, 1.0]);
        assert!(w.iter().all(|c| close(*c, 1.0, 1e-9)));
    }

    #[test]
    fn dci_white_adapts_to_d65() {
        let m = rgb_to_rgb(&named_primaries(8).unwrap(), &BT709);
        let w = mat_vec(&m, [1.0, 1.0, 1.0]);
        assert!(w.iter().all(|c| close(*c, 1.0, 1e-6)), "{w:?}");
    }

    #[test]
    fn pq_round_trips_and_hits_known_points() {
        for l in [0.0, 0.0001, 0.01, 0.0203, 0.1, 1.0] {
            assert!(close(pq_eotf(pq_inv_eotf(l)), l, 1e-9));
        }
        // 100 nits is about 0.508, 203 nits 0.58, 1000 nits 0.75 (BT.2100 tables).
        assert!(close(pq_inv_eotf(0.01), 0.5081, 1e-3));
        assert!(close(pq_inv_eotf(0.0203), 0.5806, 1e-3));
        assert!(close(pq_inv_eotf(0.1), 0.7518, 1e-3));
    }

    #[test]
    fn sdr_white_lands_on_sdr_brightness() {
        let p = HdrOutputParams {
            sdr_white: 203.0,
            peak: 400.0,
        };
        let out = encode_pq_reference([1.0, 1.0, 1.0], p);
        for c in out {
            assert!(close(c, pq_inv_eotf(0.0203), 1e-6));
        }
    }

    #[test]
    fn tonemap_keeps_below_knee_and_stays_under_peak() {
        assert_eq!(tonemap([100.0, 50.0, 0.0], 400.0), [100.0, 50.0, 0.0]);
        let t = tonemap([5000.0, 2500.0, 0.0], 400.0);
        assert!(t[0] <= 400.0 && t[0] > 300.0);
        assert!(close(t[1] / t[0], 0.5, 1e-9));
    }

    #[test]
    fn sdr_description_converts_to_nothing() {
        assert!(ConvertParams::new(&ImageDesc::sdr(), Target::Sdr).is_identity());
        assert!(ConvertParams::new(&ImageDesc::sdr(), Target::HdrIntermediate).is_identity());
        let hdr = ImageDesc::hdr(203.0, &DisplayColor::default());
        let p = ConvertParams::new(&hdr, Target::HdrIntermediate);
        assert!(!p.is_identity());
        assert!(close(p.in_scale as f64, 10000.0 / 203.0, 1e-3));
        assert!(ConvertParams::new(&hdr, Target::Sdr).peak > 1.0);
    }

    // DP-4 on the development machine (Acer XV272U W2), an HDR400 panel.
    const XV272U_EDID: &str = "00ffffffffffff000472cc0bef0430410d220104b53c22783b4f65ad5242b126\
        0e5054bfef80714f8140818081c09500b300d1c0617c565e00a0a0a029503020\
        350055502100001e000000fd0030f0ffff69010a202020202020000000fc0058\
        56323732552057320a202020000000ff00423431333030344546343232300202\
        020338f14f3f40901f2221200f0e13121103020123090707830100006d1a0000\
        020130f0000000000000e305c301e606070162621ce200d551e900a0a0a05550\
        3020350055502100001a0cdf80a0703840403040350055502100001e6fc200a0\
        a0a055503020350055502100001e0000000000000000000000000000000000fb\
        7012790300030114d9760184ff099f002f801f009f051d000200040000000000\
        0000000000000000000000000000000000000000000000000000000000000000\
        0000000000000000000000000000000000000000000000000000000000000000\
        0000000000000000000000000000000000000000000000000000000000004a90";

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn reads_hdr_info_from_a_real_edid() {
        let d = DisplayColor::from_edid(&hex(XV272U_EDID));
        assert!(d.pq, "{d:?}");
        assert!(d.bt2020, "{d:?}");
        let peak = d.max_luminance.unwrap();
        assert!((400.0..430.0).contains(&peak), "{d:?}");
        let p = d.primaries.unwrap();
        assert!(close(p.w.0, 0.3127, 0.003) && close(p.w.1, 0.329, 0.003), "{p:?}");
    }
}
