//! Screenshots and screen recording: wlr-screencopy (used by `grim`, `wf-recorder` and
//! xdg-desktop-portal-wlr for screen sharing). Frames are drawn into an offscreen texture and
//! copied into the client's shared-memory buffer.

use std::sync::Mutex;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    output::Output,
    reexports::{
        wayland_protocols_wlr::screencopy::v1::server::{
            zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
            zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
        },
    },
    utils::{Buffer, Clock, Logical, Monotonic, Point, Rectangle, Size},
    wayland::shm::with_buffer_contents_mut,
};

use crate::{config::Color, render::OutputElement, state::State};

/// The only buffer format offered. Every shm client supports it.
const SHM_FORMAT: wl_shm::Format = wl_shm::Format::Xrgb8888;

pub struct FrameData {
    manager: ZwlrScreencopyManagerV1,
    /// None when the client asked for something we can't capture; the frame already failed.
    output: Option<Output>,
    overlay_cursor: bool,
    /// The part of the output to copy, in its pixel (buffer) coordinates.
    region: Rectangle<i32, Buffer>,
    /// A frame can be copied only once.
    used: Mutex<bool>,
}

/// A `copy_with_damage` waiting for the screen to change.
pub struct PendingCopy {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
}

/// Per client and output: whether the output changed since that client's last copy.
pub struct DamageWatch {
    manager: ZwlrScreencopyManagerV1,
    output: Output,
    dirty: bool,
}

impl State {
    pub fn create_screencopy_global(dh: &DisplayHandle) {
        dh.create_global::<State, ZwlrScreencopyManagerV1, _>(3, ());
    }

    /// An output was redrawn with visible changes: hand waiting recorders their frame.
    pub fn screencopy_output_damaged(&mut self, output: &Output) {
        self.screencopy_watch.retain(|w| w.manager.is_alive());
        for w in self.screencopy_watch.iter_mut().filter(|w| &w.output == output) {
            w.dirty = true;
        }
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.screencopy_pending)
            .into_iter()
            .filter(|p| p.frame.is_alive())
            .partition(|p| frame_data(&p.frame).output.as_ref() == Some(output));
        self.screencopy_pending = waiting;
        for p in ready {
            self.copy_frame(&p.frame, &p.buffer, true);
        }
    }

    /// A monitor went away: recorders waiting on it get a failed frame instead of hanging.
    pub fn forget_screencopy_output(&mut self, output: &Output) {
        self.screencopy_watch.retain(|w| &w.output != output);
        self.screencopy_pending.retain(|p| {
            let gone = frame_data(&p.frame).output.as_ref() == Some(output);
            if gone {
                p.frame.failed();
            }
            !gone
        });
    }

    /// Render the frame's output and copy it into `buffer`, then tell the client.
    fn copy_frame(&mut self, frame: &ZwlrScreencopyFrameV1, buffer: &WlBuffer, with_damage: bool) {
        let data = frame_data(frame);
        let Some(output) = data.output.clone() else {
            frame.failed();
            return;
        };
        if let Some(w) = self
            .screencopy_watch
            .iter_mut()
            .find(|w| w.manager == data.manager && w.output == output)
        {
            w.dirty = false;
        }
        let region = data.region;
        let clear = self.config.decoration.background;
        let result = self
            .capture_output(&output, data.overlay_cursor, |renderer, elements| {
                render_to_shm(renderer, &output, elements, clear, region, buffer)
            })
            .unwrap_or_else(|| Err("the screen isn't available right now".into()));
        match result {
            Ok(()) => {
                frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
                if with_damage {
                    // The whole region; finer damage isn't worth tracking per client.
                    frame.damage(0, 0, region.size.w as u32, region.size.h as u32);
                }
                let now = std::time::Duration::from(Clock::<Monotonic>::new().now());
                let secs = now.as_secs();
                frame.ready((secs >> 32) as u32, secs as u32, now.subsec_nanos());
            }
            Err(e) => {
                tracing::warn!("screencopy of {} failed: {e}", output.name());
                frame.failed();
            }
        }
    }
}

fn frame_data(frame: &ZwlrScreencopyFrameV1) -> &FrameData {
    frame.data::<FrameData>().expect("screencopy frame without data")
}

/// Draw `elements` for the whole output offscreen, then copy `region` of it into the shm buffer.
fn render_to_shm(
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[OutputElement<GlesRenderer>],
    clear: Color,
    region: Rectangle<i32, Buffer>,
    buffer: &WlBuffer,
) -> Result<(), String> {
    let mode = output.current_mode().ok_or("output has no mode")?;
    let size: Size<i32, Buffer> = (mode.size.w, mode.size.h).into();
    let mut texture: GlesTexture = renderer
        .create_buffer(Fourcc::Abgr8888, size)
        .map_err(|e| format!("offscreen buffer: {e}"))?;
    let mut target = renderer.bind(&mut texture).map_err(|e| format!("bind: {e}"))?;
    OutputDamageTracker::from_output(output)
        .render_output(renderer, &mut target, 0, elements, clear)
        .map_err(|e| format!("render: {e:?}"))?;
    // Read back as RGBA, which every GL driver can do, and swap to the client's XRGB below.
    let mapping = renderer
        .copy_framebuffer(&target, region, Fourcc::Abgr8888)
        .map_err(|e| format!("read back: {e}"))?;
    let pixels = renderer.map_texture(&mapping).map_err(|e| format!("map: {e}"))?;

    let (w, h) = (region.size.w as usize, region.size.h as usize);
    with_buffer_contents_mut(buffer, |ptr, len, info| {
        let stride = info.stride as usize;
        let offset = info.offset as usize;
        if offset + stride * h > len {
            return Err("client buffer too small".to_string());
        }
        // SAFETY: bounds checked above; the client may scribble on its own buffer meanwhile,
        // which only garbles its own screenshot.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr.add(offset), stride * h) };
        for (y, row) in dst.chunks_exact_mut(stride).enumerate() {
            let src = &pixels[y * w * 4..(y + 1) * w * 4];
            for (d, s) in row[..w * 4].chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                // RGBA bytes → XRGB8888 (little-endian B, G, R, X).
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 0xff;
            }
        }
        Ok(())
    })
    .map_err(|e| format!("client buffer: {e:?}"))?
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        use zwlr_screencopy_manager_v1::Request;
        let (frame, overlay_cursor, wl_output, logical) = match request {
            Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => (frame, overlay_cursor, output, None),
            Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => (
                frame,
                overlay_cursor,
                output,
                Some(Rectangle::<i32, Logical>::new((x, y).into(), (width, height).into())),
            ),
            Request::Destroy => return,
            _ => return,
        };
        let target = output_and_region(state, &wl_output, logical);
        let Some((output, region)) = target else {
            // Not a monitor we know, or an empty region: the frame fails right away.
            let frame = data_init.init(
                frame,
                FrameData {
                    manager: manager.clone(),
                    output: None,
                    overlay_cursor: false,
                    region: Rectangle::default(),
                    used: Mutex::new(true),
                },
            );
            frame.failed();
            return;
        };
        if !state
            .screencopy_watch
            .iter()
            .any(|w| &w.manager == manager && w.output == output)
        {
            state.screencopy_watch.push(DamageWatch {
                manager: manager.clone(),
                output: output.clone(),
                dirty: true,
            });
        }
        let frame = data_init.init(
            frame,
            FrameData {
                manager: manager.clone(),
                output: Some(output),
                overlay_cursor: overlay_cursor != 0,
                region,
                used: Mutex::new(false),
            },
        );
        let (w, h) = (region.size.w as u32, region.size.h as u32);
        frame.buffer(SHM_FORMAT, w, h, w * 4);
        if frame.version() >= 3 {
            frame.buffer_done();
        }
    }
}

/// The monitor behind a `wl_output`, and the region to copy in its pixel coordinates.
fn output_and_region(
    state: &State,
    wl_output: &WlOutput,
    logical: Option<Rectangle<i32, Logical>>,
) -> Option<(Output, Rectangle<i32, Buffer>)> {
    let output = Output::from_resource(wl_output).filter(|o| state.monitor_index(o).is_some())?;
    let mode = output.current_mode()?;
    let full = Rectangle::<i32, Buffer>::from_size((mode.size.w, mode.size.h).into());
    let Some(logical) = logical else {
        return Some((output, full));
    };
    let transform = output.current_transform();
    let scale = output.current_scale().fractional_scale();
    let logical_size = transform.transform_size(mode.size).to_f64().to_logical(scale);
    let rect = logical.to_f64().to_buffer(scale, transform, &logical_size);
    // Round outwards so a fractional scale never cuts off the edge the user selected.
    let loc = Point::<i32, Buffer>::from((rect.loc.x.floor() as i32, rect.loc.y.floor() as i32));
    let end = Point::<i32, Buffer>::from((
        (rect.loc.x + rect.size.w).ceil() as i32,
        (rect.loc.y + rect.size.h).ceil() as i32,
    ));
    let region = Rectangle::from_extremities(loc, end).intersection(full)?;
    (!region.is_empty()).then_some((output, region))
}

impl Dispatch<ZwlrScreencopyFrameV1, FrameData> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use zwlr_screencopy_frame_v1::{Error, Request};
        let (buffer, with_damage) = match request {
            Request::Copy { buffer } => (buffer, false),
            Request::CopyWithDamage { buffer } => (buffer, true),
            _ => return,
        };
        let Some(output) = &data.output else {
            // Already failed; nothing to copy.
            return;
        };
        {
            let mut used = data.used.lock().unwrap();
            if *used {
                frame.post_error(Error::AlreadyUsed, "frame was already copied");
                return;
            }
            *used = true;
        }
        if let Err(msg) = check_buffer(&buffer, data.region.size) {
            frame.post_error(Error::InvalidBuffer, msg);
            return;
        }
        let dirty = state
            .screencopy_watch
            .iter()
            .find(|w| w.manager == data.manager && &w.output == output)
            .is_none_or(|w| w.dirty);
        if with_damage && !dirty {
            // Nothing new on screen since this client's last frame: wait for a change.
            state.screencopy_pending.push(PendingCopy {
                frame: frame.clone(),
                buffer,
            });
        } else {
            state.copy_frame(frame, &buffer, with_damage);
        }
    }

    fn destroyed(
        state: &mut State,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        frame: &ZwlrScreencopyFrameV1,
        _data: &FrameData,
    ) {
        state.screencopy_pending.retain(|p| &p.frame != frame);
    }
}

/// The client's buffer must be shm in the format and size we announced.
fn check_buffer(buffer: &WlBuffer, size: Size<i32, Buffer>) -> Result<(), &'static str> {
    smithay::wayland::shm::with_buffer_contents(buffer, |_, _, info| {
        if info.format != SHM_FORMAT {
            Err("buffer format must be xrgb8888")
        } else if info.width != size.w || info.height != size.h {
            Err("buffer size doesn't match the frame")
        } else if info.stride < size.w * 4 {
            Err("buffer stride too small")
        } else {
            Ok(())
        }
    })
    .unwrap_or(Err("only shm buffers are supported"))
}
