//! wp_color_management_v1: apps learn what each monitor expects (SDR, or HDR10 with its
//! brightness range) and say how their own pixels are meant (sRGB, PQ HDR video, scRGB games,
//! wide-gamut photos). Described surfaces are converted when drawn (see `hdr.rs`); everything
//! else is taken as sRGB.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use smithay::{
    backend::renderer::element::Id,
    output::Output,
    reexports::{
        wayland_protocols::wp::color_management::v1::server::{
            wp_color_management_output_v1::{self, WpColorManagementOutputV1},
            wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
            wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
            wp_color_manager_v1::{self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1},
            wp_image_description_creator_icc_v1::{self, WpImageDescriptionCreatorIccV1},
            wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
            wp_image_description_info_v1::WpImageDescriptionInfoV1,
            wp_image_description_reference_v1::{self, WpImageDescriptionReferenceV1},
            wp_image_description_v1::{self, Cause, WpImageDescriptionV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum, Weak,
            backend::ClientId,
            protocol::{wl_output::WlOutput, wl_surface::WlSurface},
        },
    },
    wayland::compositor::{Cacheable, get_parent, with_states},
};

use crate::{
    color::{self, Chromaticities, ImageDesc, Luminances, Tf},
    state::State,
};

/// The protocol version we speak: 2 adds 64-bit image description ids.
const VERSION: u32 = 2;

/// Transfer functions apps may describe content with, by protocol value.
fn supported_tf(value: u32, version: u32) -> Option<(Tf, bool)> {
    Some(match value {
        1 => (Tf::Power(2.4), true), // BT.1886
        2 => (Tf::Power(2.2), false),
        3 => (Tf::Power(2.8), false),
        5 => (Tf::Linear, false),
        9 if version < 2 => (Tf::Srgb, false),
        10 if version < 2 => (Tf::ExtSrgb, false),
        11 => (Tf::Pq, false),
        14 if version >= 2 => (Tf::Srgb, false), // compound power 2.4: the sRGB encoding
        _ => return None,
    })
}

const SUPPORTED_INTENTS: [RenderIntent; 2] = [RenderIntent::Perceptual, RenderIntent::Relative];

/// A surface's image description, as of its last commit.
#[derive(Debug, Clone, Default)]
pub struct ColorSurfaceState {
    pub desc: Option<Arc<ImageDesc>>,
}

impl Cacheable for ColorSurfaceState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        self.clone()
    }
    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// One wp_color_management_surface_v1 per surface at most.
#[derive(Default)]
struct HasColorSurface(AtomicBool);

#[derive(Default)]
pub struct ColorManagement {
    /// Every image description handed out so far, and its id. Ids are never reused.
    records: Vec<(Arc<ImageDesc>, u64)>,
    outputs: Vec<OutputWatch>,
    feedbacks: Vec<WpColorManagementSurfaceFeedbackV1>,
    surfaces: Vec<Weak<WlSurface>>,
    /// Information objects to fill. Sent after the request that made them, since the last
    /// event destroys the object, which can't happen while it's still being created.
    pending_info: Vec<(WpImageDescriptionInfoV1, Arc<ImageDesc>)>,
}

struct OutputWatch {
    resource: WpColorManagementOutputV1,
    wl_output: WlOutput,
    output: Output,
    identity: u64,
}

pub struct ImageDescData {
    /// None: the description failed and can't be used.
    desc: Option<Arc<ImageDesc>>,
    /// Descriptions of outputs and preferred ones may be read back; apps' own may not.
    info_allowed: bool,
}

pub struct FeedbackData {
    surface: Weak<WlSurface>,
    identity: Mutex<u64>,
}

#[derive(Default)]
pub struct ParamsData(Mutex<Params>);

#[derive(Default)]
struct Params {
    tf: Option<(Tf, Option<u32>, bool)>,
    primaries: Option<(Chromaticities, Option<u32>)>,
    lum: Option<Luminances>,
    target_primaries: Option<Chromaticities>,
    target_lum: Option<(f64, f64)>,
    max_cll: Option<f64>,
    max_fall: Option<f64>,
}

impl ColorManagement {
    /// The id of `desc`, the same for equal descriptions.
    fn intern(&mut self, desc: ImageDesc) -> (Arc<ImageDesc>, u64) {
        if let Some((d, id)) = self.records.iter().find(|(d, _)| **d == desc) {
            return (d.clone(), *id);
        }
        let id = self.records.len() as u64 + 1;
        let d = Arc::new(desc);
        self.records.push((d.clone(), id));
        (d, id)
    }
}

impl State {
    pub fn create_color_management_global(dh: &DisplayHandle) {
        dh.create_global::<State, WpColorManagerV1, _>(VERSION, ());
    }

    /// What `output` expects: HDR10 if it's driven in HDR, else ordinary sRGB.
    pub fn output_image_desc(&self, output: &Output) -> ImageDesc {
        match self.output_hdr(output) {
            Some((params, display)) => ImageDesc::hdr(params.sdr_white, &display),
            None => ImageDesc::sdr(),
        }
    }

    /// Surfaces that described their colours, by the element id they're drawn with.
    pub fn color_described_surfaces(&self) -> Vec<(Id, ImageDesc)> {
        self.color
            .surfaces
            .iter()
            .filter_map(|w| w.upgrade().ok())
            .filter_map(|s| {
                let desc = with_states(&s, |st| {
                    st.cached_state.get::<ColorSurfaceState>().current().desc.clone()
                })?;
                Some((Id::from_wayland_resource(&s), (*desc).clone()))
            })
            .collect()
    }

    /// The monitor a surface is (mostly) on: its window's, else the focused one.
    fn surface_output(&self, surface: &WlSurface) -> Option<Output> {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        self.window_for_surface(&root)
            .and_then(|w| self.monitor_of(&w))
            .map(|i| self.monitors[i].output.clone())
            .or_else(|| self.output())
    }

    fn preferred_desc(&self, surface: &WlSurface) -> ImageDesc {
        self.surface_output(surface)
            .map_or_else(ImageDesc::sdr, |o| self.output_image_desc(&o))
    }

    /// Tell apps when a monitor's description, or the one preferred for their surface, changed
    /// (HDR switched on or off, a window moved to another monitor). Run once per loop pass.
    pub fn refresh_color_feedback(&mut self) {
        for (info, desc) in std::mem::take(&mut self.color.pending_info) {
            if info.is_alive() {
                send_info(&info, &desc);
            }
        }
        self.color.surfaces.retain(|w| w.upgrade().is_ok());
        self.color.outputs.retain(|o| o.resource.is_alive());
        self.color.feedbacks.retain(|f| f.is_alive());
        if self.color.outputs.is_empty() && self.color.feedbacks.is_empty() {
            return;
        }
        for i in 0..self.color.outputs.len() {
            let output = self.color.outputs[i].output.clone();
            if self.monitor_index(&output).is_none() {
                continue;
            }
            let desc = self.output_image_desc(&output);
            let (_, id) = self.color.intern(desc);
            let watch = &mut self.color.outputs[i];
            if watch.identity != id {
                watch.identity = id;
                watch.resource.image_description_changed();
                if watch.wl_output.version() >= 2 {
                    watch.wl_output.done();
                }
            }
        }
        for f in self.color.feedbacks.clone() {
            let data = f.data::<FeedbackData>().unwrap();
            let Ok(surface) = data.surface.upgrade() else { continue };
            let desc = self.preferred_desc(&surface);
            let (_, id) = self.color.intern(desc);
            let mut last = data.identity.lock().unwrap();
            if *last != id {
                *last = id;
                if f.version() >= 2 {
                    f.preferred_changed2((id >> 32) as u32, id as u32);
                } else {
                    f.preferred_changed(id as u32);
                }
            }
        }
    }
}

/// Answer a new image description object: ready, with its id.
fn send_ready(obj: &WpImageDescriptionV1, id: u64) {
    if obj.version() >= 2 {
        obj.ready2((id >> 32) as u32, id as u32);
    } else {
        obj.ready(id as u32);
    }
}

fn init_desc(
    state: &mut State,
    data_init: &mut DataInit<'_, State>,
    new: New<WpImageDescriptionV1>,
    desc: ImageDesc,
    info_allowed: bool,
) {
    let (desc, id) = state.color.intern(desc);
    let obj = data_init.init(
        new,
        ImageDescData {
            desc: Some(desc),
            info_allowed,
        },
    );
    send_ready(&obj, id);
}

fn init_failed(data_init: &mut DataInit<'_, State>, new: New<WpImageDescriptionV1>, cause: Cause, msg: &str) {
    let obj = data_init.init(
        new,
        ImageDescData {
            desc: None,
            info_allowed: false,
        },
    );
    obj.failed(cause, msg.into());
}

impl GlobalDispatch<WpColorManagerV1, ()> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WpColorManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        let m = data_init.init(resource, ());
        for intent in SUPPORTED_INTENTS {
            m.supported_intent(intent);
        }
        for f in [
            Feature::Parametric,
            Feature::SetPrimaries,
            Feature::SetTfPower,
            Feature::SetLuminances,
            Feature::SetMasteringDisplayPrimaries,
            Feature::ExtendedTargetVolume,
            Feature::WindowsScrgb,
        ] {
            m.supported_feature(f);
        }
        for tf in 1..=14 {
            if supported_tf(tf, m.version()).is_some()
                && let Ok(tf) = TransferFunction::try_from(tf)
            {
                m.supported_tf_named(tf);
            }
        }
        for p in 1..=10 {
            if let Ok(p) = Primaries::try_from(p) {
                m.supported_primaries_named(p);
            }
        }
        m.done();
    }
}

impl Dispatch<WpColorManagerV1, ()> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        manager: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_manager_v1::{Error, Request};
        match request {
            Request::GetOutput { id, output } => {
                let resource = data_init.init(id, Output::from_resource(&output));
                if let Some(o) = Output::from_resource(&output) {
                    let (_, identity) = state.color.intern(state.output_image_desc(&o));
                    state.color.outputs.push(OutputWatch {
                        resource,
                        wl_output: output,
                        output: o,
                        identity,
                    });
                }
            }
            Request::GetSurface { id, surface } => {
                let taken = with_states(&surface, |s| {
                    s.data_map.insert_if_missing_threadsafe(HasColorSurface::default);
                    s.data_map
                        .get::<HasColorSurface>()
                        .unwrap()
                        .0
                        .swap(true, Ordering::AcqRel)
                });
                if taken {
                    manager.post_error(
                        Error::SurfaceExists,
                        "the surface already has a color management object",
                    );
                    return;
                }
                state.color.surfaces.push(surface.downgrade());
                data_init.init(id, surface.downgrade());
            }
            Request::GetSurfaceFeedback { id, surface } => {
                let (_, identity) = state.color.intern(state.preferred_desc(&surface));
                let f = data_init.init(
                    id,
                    FeedbackData {
                        surface: surface.downgrade(),
                        identity: Mutex::new(identity),
                    },
                );
                state.color.feedbacks.push(f);
            }
            Request::CreateIccCreator { obj } => {
                data_init.init(obj, ());
                manager.post_error(Error::UnsupportedFeature, "ICC profiles aren't supported");
            }
            Request::CreateParametricCreator { obj } => {
                data_init.init(obj, ParamsData::default());
            }
            Request::CreateWindowsScrgb { image_description } => {
                // Extended linear BT.709 where 1.0 is 80 nits and SDR white sits at 203.
                let desc = ImageDesc {
                    tf: Tf::Linear,
                    tf_named: Some(5),
                    primaries: color::BT709,
                    primaries_named: Some(color::PRIMARIES_SRGB),
                    lum: Luminances {
                        min: 0.0,
                        max: 80.0,
                        reference: 203.0,
                    },
                    target_primaries: None,
                    target_lum: None,
                    max_cll: None,
                    max_fall: None,
                };
                init_desc(state, data_init, image_description, desc, false);
            }
            Request::GetImageDescription { image_description, .. } => {
                // References come from other protocols, none of which we offer.
                init_failed(data_init, image_description, Cause::Unsupported, "unknown reference");
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementOutputV1, Option<Output>> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        _resource: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        output: &Option<Output>,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        if let wp_color_management_output_v1::Request::GetImageDescription { image_description } = request {
            match output.as_ref().filter(|o| state.monitor_index(o).is_some()) {
                Some(o) => {
                    let desc = state.output_image_desc(o);
                    init_desc(state, data_init, image_description, desc, true);
                }
                None => init_failed(data_init, image_description, Cause::NoOutput, "the monitor is gone"),
            }
        }
    }
}

impl Dispatch<WpColorManagementSurfaceV1, Weak<WlSurface>> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        resource: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        surface: &Weak<WlSurface>,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_management_surface_v1::{Error, Request};
        let surface = surface.upgrade().ok();
        let desc = match request {
            Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                let Some(_) = &surface else {
                    resource.post_error(Error::Inert, "the surface is gone");
                    return;
                };
                let intent_ok = matches!(render_intent, WEnum::Value(i) if SUPPORTED_INTENTS.contains(&i));
                if !intent_ok {
                    resource.post_error(Error::RenderIntent, "unsupported rendering intent");
                    return;
                }
                let Some(desc) = image_description.data::<ImageDescData>().and_then(|d| d.desc.clone()) else {
                    resource.post_error(Error::ImageDescription, "the image description isn't ready");
                    return;
                };
                Some(desc)
            }
            Request::UnsetImageDescription => {
                if surface.is_none() {
                    resource.post_error(Error::Inert, "the surface is gone");
                    return;
                }
                None
            }
            Request::Destroy => {
                if let Some(s) = &surface {
                    with_states(s, |st| {
                        if let Some(h) = st.data_map.get::<HasColorSurface>() {
                            h.0.store(false, Ordering::Release);
                        }
                    });
                }
                None
            }
            _ => return,
        };
        if let Some(s) = surface {
            with_states(&s, |st| {
                st.cached_state.get::<ColorSurfaceState>().pending().desc = desc
            });
        }
    }
}

impl Dispatch<WpColorManagementSurfaceFeedbackV1, FeedbackData> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        data: &FeedbackData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_color_management_surface_feedback_v1::{Error, Request};
        let new = match request {
            Request::GetPreferred { image_description } => image_description,
            Request::GetPreferredParametric { image_description } => image_description,
            _ => return,
        };
        let Ok(surface) = data.surface.upgrade() else {
            data_init.init(
                new,
                ImageDescData {
                    desc: None,
                    info_allowed: false,
                },
            );
            resource.post_error(Error::Inert, "the surface is gone");
            return;
        };
        let desc = state.preferred_desc(&surface);
        init_desc(state, data_init, new, desc, true);
    }
}

impl Dispatch<WpImageDescriptionCreatorIccV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &WpImageDescriptionCreatorIccV1,
        request: wp_image_description_creator_icc_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        // Never reached in practice: creating the creator is already an error.
        if let wp_image_description_creator_icc_v1::Request::Create { image_description } = request {
            init_failed(
                data_init,
                image_description,
                Cause::Unsupported,
                "ICC profiles aren't supported",
            );
        }
    }
}

fn xy(x: i32, y: i32) -> (f64, f64) {
    (x as f64 / 1e6, y as f64 / 1e6)
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, ParamsData> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        data: &ParamsData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_image_description_creator_params_v1::{Error, Request};
        let mut p = data.0.lock().unwrap();
        let already = |r: &WpImageDescriptionCreatorParamsV1| r.post_error(Error::AlreadySet, "already set");
        match request {
            Request::SetTfNamed { tf } => {
                let value = match tf {
                    WEnum::Value(v) => u32::from(v),
                    WEnum::Unknown(v) => v,
                };
                let Some((tf, bt1886)) = supported_tf(value, resource.version()) else {
                    resource.post_error(Error::InvalidTf, "unsupported transfer function");
                    return;
                };
                if p.tf.is_some() {
                    return already(resource);
                }
                p.tf = Some((tf, Some(value), bt1886));
            }
            Request::SetTfPower { eexp } => {
                if !(10000..=100000).contains(&eexp) {
                    resource.post_error(Error::InvalidTf, "exponent must be between 1 and 10");
                    return;
                }
                if p.tf.is_some() {
                    return already(resource);
                }
                p.tf = Some((Tf::Power(eexp as f64 / 10000.0), None, false));
            }
            Request::SetPrimariesNamed { primaries } => {
                let value = match primaries {
                    WEnum::Value(v) => u32::from(v),
                    WEnum::Unknown(v) => v,
                };
                let Some(c) = color::named_primaries(value) else {
                    resource.post_error(Error::InvalidPrimariesNamed, "unknown primaries");
                    return;
                };
                if p.primaries.is_some() {
                    return already(resource);
                }
                p.primaries = Some((c, Some(value)));
            }
            Request::SetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                if p.primaries.is_some() {
                    return already(resource);
                }
                let c = Chromaticities {
                    r: xy(r_x, r_y),
                    g: xy(g_x, g_y),
                    b: xy(b_x, b_y),
                    w: xy(w_x, w_y),
                };
                p.primaries = Some((c, None));
            }
            Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                let min = min_lum as f64 / 10000.0;
                if max_lum as f64 <= min || reference_lum as f64 <= min {
                    resource.post_error(Error::InvalidLuminance, "max and reference must exceed min");
                    return;
                }
                if p.lum.is_some() {
                    return already(resource);
                }
                p.lum = Some(Luminances {
                    min,
                    max: max_lum as f64,
                    reference: reference_lum as f64,
                });
            }
            Request::SetMasteringDisplayPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                if p.target_primaries.is_some() {
                    return already(resource);
                }
                p.target_primaries = Some(Chromaticities {
                    r: xy(r_x, r_y),
                    g: xy(g_x, g_y),
                    b: xy(b_x, b_y),
                    w: xy(w_x, w_y),
                });
            }
            Request::SetMasteringLuminance { min_lum, max_lum } => {
                let min = min_lum as f64 / 10000.0;
                if max_lum as f64 <= min {
                    resource.post_error(Error::InvalidLuminance, "max must exceed min");
                    return;
                }
                if p.target_lum.is_some() {
                    return already(resource);
                }
                p.target_lum = Some((min, max_lum as f64));
            }
            Request::SetMaxCll { max_cll } => {
                if p.max_cll.is_some() {
                    return already(resource);
                }
                p.max_cll = Some(max_cll as f64);
            }
            Request::SetMaxFall { max_fall } => {
                if p.max_fall.is_some() {
                    return already(resource);
                }
                p.max_fall = Some(max_fall as f64);
            }
            Request::Create { image_description } => {
                let (Some((tf, tf_named, bt1886)), Some((primaries, primaries_named))) = (p.tf, p.primaries) else {
                    resource.post_error(Error::IncompleteSet, "transfer function and primaries are required");
                    return;
                };
                if let (Some(cll), Some(fall)) = (p.max_cll, p.max_fall)
                    && fall > cll
                {
                    resource.post_error(Error::InvalidLuminance, "max_fall exceeds max_cll");
                    return;
                }
                if resource.version() < 2
                    && let Some((min, max)) = p.target_lum
                    && [p.max_cll, p.max_fall]
                        .into_iter()
                        .flatten()
                        .any(|v| v <= min || v > max)
                {
                    resource.post_error(Error::InvalidLuminance, "max_cll/max_fall outside the mastering range");
                    return;
                }
                let mut lum = p.lum.unwrap_or_else(|| Luminances::default_for(tf, bt1886));
                if tf == Tf::Pq {
                    lum.max = lum.min + 10000.0;
                }
                let desc = ImageDesc {
                    tf,
                    tf_named,
                    primaries,
                    primaries_named,
                    lum,
                    target_primaries: p.target_primaries,
                    target_lum: p.target_lum,
                    max_cll: p.max_cll,
                    max_fall: p.max_fall,
                };
                drop(p);
                init_desc(state, data_init, image_description, desc, false);
            }
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionV1, ImageDescData> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        data: &ImageDescData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use wp_image_description_v1::{Error, Request};
        if let Request::GetInformation { information } = request {
            let info = data_init.init(information, ());
            let Some(desc) = &data.desc else {
                resource.post_error(Error::NotReady, "the image description isn't ready");
                return;
            };
            if !data.info_allowed {
                resource.post_error(Error::NoInformation, "no information for this image description");
                return;
            }
            state.color.pending_info.push((info, desc.clone()));
        }
    }
}

fn send_info(info: &WpImageDescriptionInfoV1, d: &ImageDesc) {
    let m = |v: f64| (v * 1e6).round() as i32;
    let send_primaries = |c: &Chromaticities, target: bool| {
        let args = (
            m(c.r.0),
            m(c.r.1),
            m(c.g.0),
            m(c.g.1),
            m(c.b.0),
            m(c.b.1),
            m(c.w.0),
            m(c.w.1),
        );
        if target {
            info.target_primaries(args.0, args.1, args.2, args.3, args.4, args.5, args.6, args.7);
        } else {
            info.primaries(args.0, args.1, args.2, args.3, args.4, args.5, args.6, args.7);
        }
    };
    send_primaries(&d.primaries, false);
    if let Some(p) = d.primaries_named.and_then(|p| Primaries::try_from(p).ok()) {
        info.primaries_named(p);
    }
    match (d.tf_named.and_then(|t| TransferFunction::try_from(t).ok()), d.tf) {
        (Some(t), _) => info.tf_named(t),
        (None, Tf::Power(e)) => info.tf_power((e * 10000.0).round() as u32),
        (None, _) => {}
    }
    info.luminances(
        (d.lum.min * 10000.0).round() as u32,
        d.lum.max.round() as u32,
        d.lum.reference.round() as u32,
    );
    send_primaries(&d.target_primaries.unwrap_or(d.primaries), true);
    let (tmin, tmax) = d.target_lum.unwrap_or((d.lum.min, d.lum.max));
    info.target_luminance((tmin * 10000.0).round() as u32, tmax.round() as u32);
    if let Some(v) = d.max_cll {
        info.target_max_cll(v.round() as u32);
    }
    if let Some(v) = d.max_fall {
        info.target_max_fall(v.round() as u32);
    }
    info.done();
}

impl Dispatch<WpImageDescriptionInfoV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &WpImageDescriptionInfoV1,
        _request: <WpImageDescriptionInfoV1 as Resource>::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
    }
}

impl Dispatch<WpImageDescriptionReferenceV1, ()> for State {
    fn request(
        _state: &mut State,
        _client: &Client,
        _resource: &WpImageDescriptionReferenceV1,
        _request: wp_image_description_reference_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
    }

    fn destroyed(_state: &mut State, _client: ClientId, _resource: &WpImageDescriptionReferenceV1, _data: &()) {}
}
