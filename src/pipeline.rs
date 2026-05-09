use anyhow::{Context, Ok, Result, bail, ensure};
use gst::prelude::*;
use gtk::graphene::Rect;

use std::{os::unix::io::RawFd, path::PathBuf};

use crate::{
    area_selector::SelectAreaData,
    device::{self, DeviceClass},
    profile::Profile,
    screencast_portal::Stream,
};

const AUDIO_SAMPLE_RATE: i32 = 48_000;

/// Target buffer duration in front of the encoder. Sized to absorb I-frame
/// encode hiccups (typically 30–80 ms) plus a safety margin; if the encoder
/// is sustained-slow, the queue fills, `pipewiresrc` blocks, and `PipeWire`
/// drops at the source — the right failure mode.
const VIDEOENC_QUEUE_TARGET_SECONDS: u64 = 2;
/// Absolute memory ceiling for the raw-video queue regardless of resolution.
/// Keeps the recorder from monopolising system RAM if the encoder freezes;
/// 1.5 GiB is safe headroom on a 16 GiB machine while still buffering ~1 s
/// of `4K` `BGRx` (~33 MiB/frame).
const VIDEOENC_QUEUE_BYTES_CEILING: u32 = 1_536 * 1024 * 1024;
/// Floor so very low resolutions still get a meaningful buffer.
const VIDEOENC_QUEUE_BYTES_FLOOR: u32 = 64 * 1024 * 1024;
/// Fallback frame size used when stream dimensions aren't reported by the
/// portal — 4K `BGRx`. Sized large because the portal's reported stream size
/// can lie (e.g. `KWin` advertises 1080p logical even when delivering 4K),
/// and over-provisioning the byte cap costs nothing — the queue only holds
/// the buffers actually pushed into it. The cap is re-tightened from the
/// real negotiated caps via a pad probe once the first caps event flows.
const VIDEOENC_QUEUE_FALLBACK_FRAME_BYTES: u64 = 3840 * 2160 * 4;

#[derive(Debug)]
#[must_use]
pub struct PipelineBuilder {
    file_path: PathBuf,
    framerate: gst::Fraction,
    profile: Profile,
    fd: RawFd,
    streams: Vec<Stream>,
    record_desktop_audio: bool,
    record_microphone: bool,
    select_area_data: Option<SelectAreaData>,
    resolution_height: u32,
    audio_codec_id: String,
}

impl PipelineBuilder {
    pub fn new(
        file_path: PathBuf,
        framerate: gst::Fraction,
        profile: Profile,
        fd: RawFd,
        streams: Vec<Stream>,
    ) -> Self {
        Self {
            file_path,
            framerate,
            profile,
            fd,
            streams,
            record_desktop_audio: false,
            record_microphone: false,
            select_area_data: None,
            resolution_height: 0,
            audio_codec_id: String::new(),
        }
    }

    /// Sets the target output height. `0` means use the source resolution.
    pub fn resolution_height(&mut self, height: u32) -> &mut Self {
        self.resolution_height = height;
        self
    }

    /// Sets the preferred audio codec id (e.g. "opus", "aac"). The profile
    /// silently falls back to its first listed codec when this isn't supported.
    pub fn audio_codec_id(&mut self, id: impl Into<String>) -> &mut Self {
        self.audio_codec_id = id.into();
        self
    }

    pub fn record_desktop_audio(&mut self, record_desktop_audio: bool) -> &mut Self {
        self.record_desktop_audio = record_desktop_audio;
        self
    }

    pub fn record_microphone(&mut self, record_microphone: bool) -> &mut Self {
        self.record_microphone = record_microphone;
        self
    }

    pub fn select_area_data(&mut self, data: SelectAreaData) -> &mut Self {
        self.select_area_data = Some(data);
        self
    }

    /// Builds the pipeline.
    ///
    ///                   (If has select_area_data)
    ///                        |             |
    ///                        v             v
    /// pipewiresrc-bin -> videoscale -> videocrop -> queue -> |
    ///                                                        | -> profile.attach -> filesink
    ///                               pulsesrc-bin -> queue -> |
    pub fn build(&self) -> Result<gst::Pipeline> {
        tracing::debug!(
            file_path = %self.file_path.display(),
            framerate = ?self.framerate,
            profile = ?self.profile.id(),
            fd = self.fd,
            stream_len = self.streams.len(),
            streams = ?self.streams,
            record_desktop_audio = ?self.record_desktop_audio,
            record_microphone = ?self.record_microphone,
            select_area_data = ?self.select_area_data,
        );

        let pipeline = gst::Pipeline::new();

        let videosrc_bin = make_videosrc_bin(
            self.fd,
            &self.streams,
            self.framerate,
            self.resolution_height,
        )
        .context("Failed to create videosrc bin")?;
        let videoenc_queue_initial_max_bytes = compute_videoenc_queue_max_bytes(
            &self.streams,
            self.framerate,
            VIDEOENC_QUEUE_TARGET_SECONDS,
        );
        tracing::debug!(
            videoenc_queue_max_bytes = videoenc_queue_initial_max_bytes,
            target_seconds = VIDEOENC_QUEUE_TARGET_SECONDS,
            "Sized kooha-videoenc-queue (initial; will be re-tightened from negotiated caps)"
        );
        let videoenc_queue = gst::ElementFactory::make("queue")
            .name("kooha-videoenc-queue")
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", videoenc_queue_initial_max_bytes)
            .property(
                "max-size-time",
                VIDEOENC_QUEUE_TARGET_SECONDS * gst::ClockTime::SECOND.nseconds(),
            )
            .build()?;
        attach_videoenc_queue_resize_probe(&videoenc_queue, self.framerate);
        let filesink = gst::ElementFactory::make("filesink")
            .property(
                "location",
                self.file_path
                    .to_str()
                    .context("Could not convert file path to string")?,
            )
            .build()?;
        pipeline.add_many([videosrc_bin.upcast_ref(), &videoenc_queue, &filesink])?;

        if let Some(ref data) = self.select_area_data {
            let videoscale = gst::ElementFactory::make("videoscale").build()?;
            let videocrop = make_videocrop(data)?;
            pipeline.add_many([&videoscale, &videocrop])?;

            // x264enc requires even resolution.
            let (stream_width, stream_height) = data.stream_size;
            let videoscale_caps = gst::Caps::builder("video/x-raw")
                .field("width", round_to_even(stream_width))
                .field("height", round_to_even(stream_height))
                .build();

            videosrc_bin.link(&videoscale)?;
            videoscale.link_filtered(&videocrop, &videoscale_caps)?;
            videocrop.link(&videoenc_queue)?;
        } else {
            videosrc_bin.link(&videoenc_queue)?;
        }

        let audioenc_queue = if self.record_desktop_audio || self.record_microphone {
            debug_assert!(self.profile.supports_audio());

            let pulsesrcs = [
                self.record_desktop_audio
                    .then(|| make_pulsesrc(DeviceClass::Sink, "kooha-desktop-audio-src")),
                self.record_microphone
                    .then(|| make_pulsesrc(DeviceClass::Source, "kooha-microphone-src")),
            ];
            let audiosrc_bin = make_audiosrc_bin(
                &pulsesrcs
                    .into_iter()
                    .flatten()
                    .collect::<Result<Vec<_>>>()?,
            )
            .context("Failed to create audiosrc bin")?;
            let audioenc_queue = gst::ElementFactory::make("queue")
                .name("kooha-audioenc-queue")
                .build()?;

            pipeline.add_many([audiosrc_bin.upcast_ref(), &audioenc_queue])?;
            audiosrc_bin.link(&audioenc_queue)?;

            Some(audioenc_queue)
        } else {
            None
        };

        self.profile
            .attach(
                &pipeline,
                &videoenc_queue,
                audioenc_queue.as_ref(),
                &filesink,
                &self.audio_codec_id,
            )
            .with_context(|| {
                format!(
                    "Failed to attach profile `{}` to pipeline",
                    self.profile.id()
                )
            })?;

        Ok(pipeline)
    }
}

/// Builds a `pipewiresrc` followed by a capsfilter that pins the negotiated
/// framerate. Without the pin, `pipewiresrc` accepts whatever default the
/// portal advertises — KWin's screencast on Wayland defaults to ~30 fps at
/// 4K regardless of the consumer's downstream capsfilter, because the rate
/// is fixed at PipeWire stream-format negotiation time, before downstream
/// caps have been seen. With the pin, KWin delivers frames at the requested
/// rate and `videorate` further down becomes a near no-op.
///
/// Returns the pair as a `(src, capsfilter)` so the caller can both add them
/// to the bin and link them in order.
fn make_pipewiresrc(
    fd: RawFd,
    path: &str,
    framerate: gst::Fraction,
) -> Result<(gst::Element, gst::Element)> {
    let src = gst::ElementFactory::make("pipewiresrc")
        .property("fd", fd)
        .property("path", path)
        .property("do-timestamp", true)
        .property("provide-clock", false)
        .property("keepalive-time", 1000)
        .property("resend-last", true)
        .build()?;

    // Use a range `[1/1, N/1]` rather than pinning `framerate=N/1`. KWin
    // (and likely other portals) advertises a fixed list of supported rates
    // — pinning a value not on the list breaks negotiation entirely. The
    // range intersects with the portal's list so the portal picks the
    // highest rate it can deliver up to N. videorate downstream still
    // converts to exactly N.
    //
    // Caps are built with ANY features so the constraint applies regardless
    // of memory feature. `pipewiresrc` emits `video/x-raw(memory:DMABuf)`
    // on KWin/NVIDIA Wayland — a plain `video/x-raw` capsfilter (which
    // implicitly means `memory:SystemMemory`) does not intersect with that
    // and breaks negotiation.
    let one = gst::Fraction::new(1, 1);
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder_full_with_any_features()
                .structure(
                    gst::Structure::builder("video/x-raw")
                        .field("framerate", gst::FractionRange::new(one, framerate))
                        .build(),
                )
                .build(),
        )
        .build()?;

    Ok((src, capsfilter))
}

fn make_videoflip() -> Result<gst::Element> {
    let videoflip = gst::ElementFactory::make("videoflip")
        .property_from_str("video-direction", "auto")
        .build()?;

    Ok(videoflip)
}

/// Create a videocrop element that computes the crop from the given coordinates
/// and size.
fn make_videocrop(data: &SelectAreaData) -> Result<gst::Element> {
    let SelectAreaData {
        selection,
        paintable_rect,
        stream_size,
    } = data;

    let (stream_width, stream_height) = stream_size;
    let scale_factor_h = *stream_width as f32 / paintable_rect.width();
    let scale_factor_v = *stream_height as f32 / paintable_rect.height();

    if scale_factor_h != scale_factor_v {
        tracing::warn!(
            scale_factor_h,
            scale_factor_v,
            "Scale factors of horizontal and vertical are unequal"
        );
    }

    // Both paintable and selection position are relative to the widget coordinates.
    // To get the absolute position and so correct crop values, subtract the paintable
    // rect's position from the selection rect.
    let old_selection_rect = selection.rect();
    let selection_rect_scaled = Rect::new(
        old_selection_rect.x() - paintable_rect.x(),
        old_selection_rect.y() - paintable_rect.y(),
        old_selection_rect.width(),
        old_selection_rect.height(),
    )
    .scale(scale_factor_h, scale_factor_v);

    let raw_top_crop = selection_rect_scaled.y();
    let raw_left_crop = selection_rect_scaled.x();
    let raw_right_crop =
        *stream_width as f32 - (selection_rect_scaled.width() + selection_rect_scaled.x());
    let raw_bottom_crop =
        *stream_height as f32 - (selection_rect_scaled.height() + selection_rect_scaled.y());

    tracing::debug!(raw_top_crop, raw_left_crop, raw_right_crop, raw_bottom_crop);

    let top_crop = round_to_even_f32(raw_top_crop).clamp(0, *stream_height);
    let left_crop = round_to_even_f32(raw_left_crop).clamp(0, *stream_width);
    let right_crop = round_to_even_f32(raw_right_crop).clamp(0, *stream_width);
    let bottom_crop = round_to_even_f32(raw_bottom_crop).clamp(0, *stream_height);

    tracing::debug!(top_crop, left_crop, right_crop, bottom_crop);

    // x264enc requires even resolution.
    let crop = gst::ElementFactory::make("videocrop")
        .property("top", top_crop)
        .property("left", left_crop)
        .property("right", right_crop)
        .property("bottom", bottom_crop)
        .build()?;
    Ok(crop)
}

/// Creates a bin with a src pad for multiple pipewire streams.
///
/// Single stream (no `videoflip`! it only accepts sysmem and would force a
/// DMABUF→sysmem map at the source, killing the GL/CUDA zero-copy path
/// downstream — `pipewiresrc`'s output is already correctly oriented on
/// every desktop portal we care about):
///
/// pipewiresrc -> [videoscale -> capsfilter] -> videorate
///
/// Multiple streams (`compositor` only accepts sysmem, so this path is
/// inherently CPU; `videoflip` stays here because it costs nothing extra):
///
/// pipewiresrc1 -> videoflip -> |
///                              |
/// pipewiresrc2 -> videoflip -> | -> compositor -> [videoscale -> capsfilter] -> videorate
///                              |
/// pipewiresrcn -> videoflip -> |
///
/// The optional videoscale + capsfilter pair is inserted only when
/// `resolution_height` is non-zero and would actually downscale the source.
pub fn make_videosrc_bin(
    fd: RawFd,
    streams: &[Stream],
    framerate: gst::Fraction,
    resolution_height: u32,
) -> Result<gst::Bin> {
    // TODO Create a bin that hotswaps compositor depending whether gl is supported or not.

    let bin = gst::Bin::builder().name("kooha-pipewiresrc-bin").build();

    let videorate = gst::ElementFactory::make("videorate")
        .property("skip-to-first", true)
        .build()?;
    let videorate_capsfilter = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("framerate", framerate)
                .build(),
        )
        .build()?;
    bin.add_many([&videorate, &videorate_capsfilter])?;
    videorate.link(&videorate_capsfilter)?;

    let scale_target_size = composited_target_size(streams, resolution_height);
    let scale_sink: gst::Element = if let Some((target_w, target_h)) = scale_target_size {
        let videoscale = gst::ElementFactory::make("videoscale").build()?;
        let scale_capsfilter = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", target_w)
                    .field("height", target_h)
                    .field("pixel-aspect-ratio", gst::Fraction::from_integer(1))
                    .build(),
            )
            .build()?;
        bin.add_many([&videoscale, &scale_capsfilter])?;
        videoscale.link(&scale_capsfilter)?;
        scale_capsfilter.link(&videorate)?;
        videoscale
    } else {
        videorate.clone()
    };

    match streams {
        [] => bail!("No streams provided"),
        [stream] => {
            let (pipewiresrc, src_capsfilter) =
                make_pipewiresrc(fd, &stream.node_id().to_string(), framerate)?;
            bin.add_many([&pipewiresrc, &src_capsfilter])?;
            pipewiresrc.link(&src_capsfilter)?;
            src_capsfilter.link(&scale_sink)?;
        }
        streams => {
            let compositor = gst::ElementFactory::make("compositor").build()?;
            bin.add(&compositor)?;
            compositor.link(&scale_sink)?;

            let mut last_pos = 0;
            for stream in streams {
                let (pipewiresrc, src_capsfilter) =
                    make_pipewiresrc(fd, &stream.node_id().to_string(), framerate)?;
                let videoflip = make_videoflip()?;
                bin.add_many([&pipewiresrc, &src_capsfilter, &videoflip])?;
                pipewiresrc.link(&src_capsfilter)?;
                src_capsfilter.link(&videoflip)?;

                let compositor_sink_pad = compositor
                    .request_pad_simple("sink_%u")
                    .context("Failed to request sink_%u pad from compositor")?;
                compositor_sink_pad.set_property("xpos", last_pos);
                videoflip
                    .static_pad("src")
                    .unwrap()
                    .link(&compositor_sink_pad)?;

                let (stream_width, _) = stream.size().unwrap();
                last_pos += stream_width;
            }
        }
    }

    let src_pad = videorate_capsfilter.static_pad("src").unwrap();
    bin.add_pad(&gst::GhostPad::with_target(&src_pad)?)?;

    Ok(bin)
}

/// Installs a one-shot pad probe on the queue's sink. The portal's reported
/// stream size is unreliable on `KWin` (advertises logical 1080p while
/// delivering 4K via DMABUF), so the right resolution is whatever ends up
/// in the negotiated caps. On the first downstream caps event we recompute
/// `max-size-bytes` from the real frame dimensions and remove the probe.
fn attach_videoenc_queue_resize_probe(queue: &gst::Element, framerate: gst::Fraction) {
    let Some(sink_pad) = queue.static_pad("sink") else {
        tracing::warn!("kooha-videoenc-queue has no sink pad; skipping resize probe");
        return;
    };
    let queue_for_probe = queue.clone();
    sink_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
        let Some(gst::PadProbeData::Event(ref event)) = info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let caps = caps_event.caps();
        let Some(s) = caps.structure(0) else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(width) = s.get::<i32>("width").ok() else {
            return gst::PadProbeReturn::Ok;
        };
        let Some(height) = s.get::<i32>("height").ok() else {
            return gst::PadProbeReturn::Ok;
        };
        if width <= 0 || height <= 0 {
            return gst::PadProbeReturn::Ok;
        }

        let frame_bytes = (width as u64) * (height as u64) * 4;
        let fps = (framerate.numer() as f64 / framerate.denom().max(1) as f64).ceil() as u64;
        let needed = frame_bytes
            .saturating_mul(fps.max(1))
            .saturating_mul(VIDEOENC_QUEUE_TARGET_SECONDS);
        let new_max = needed.clamp(
            VIDEOENC_QUEUE_BYTES_FLOOR as u64,
            VIDEOENC_QUEUE_BYTES_CEILING as u64,
        ) as u32;

        let current_max = queue_for_probe.property::<u32>("max-size-bytes");
        if new_max == current_max {
            tracing::debug!(
                width,
                height,
                max_bytes = new_max,
                "kooha-videoenc-queue already correctly sized"
            );
        } else {
            tracing::debug!(
                width,
                height,
                old_max_bytes = current_max,
                new_max_bytes = new_max,
                "Resized kooha-videoenc-queue from negotiated caps"
            );
            queue_for_probe.set_property("max-size-bytes", new_max);
        }

        gst::PadProbeReturn::Remove
    });
}

/// Estimates the composited raw-video frame size in bytes (`BGRx` = 4 bytes
/// per pixel). Falls back to a 4K estimate when stream dimensions are
/// unknown so the queue is sized for the worst common case until the real
/// caps land.
fn estimated_frame_bytes(streams: &[Stream]) -> u64 {
    match streams {
        [single] => single.size().map_or(VIDEOENC_QUEUE_FALLBACK_FRAME_BYTES, |(w, h)| {
            (w as u64) * (h as u64) * 4
        }),
        many if !many.is_empty() => {
            let mut total_w: u64 = 0;
            let mut max_h: u64 = 0;
            for s in many {
                if let Some((w, h)) = s.size() {
                    total_w += w as u64;
                    max_h = max_h.max(h as u64);
                }
            }
            if total_w == 0 || max_h == 0 {
                VIDEOENC_QUEUE_FALLBACK_FRAME_BYTES
            } else {
                total_w * max_h * 4
            }
        }
        _ => VIDEOENC_QUEUE_FALLBACK_FRAME_BYTES,
    }
}

/// Sizes the raw-video queue's byte cap to hold roughly `target_seconds` of
/// frames at the source resolution and target framerate, clamped between the
/// floor and ceiling so very low or very high resolutions both get sane caps.
fn compute_videoenc_queue_max_bytes(
    streams: &[Stream],
    framerate: gst::Fraction,
    target_seconds: u64,
) -> u32 {
    let frame_bytes = estimated_frame_bytes(streams);
    let fps = (framerate.numer() as f64 / framerate.denom().max(1) as f64).ceil() as u64;
    let needed = frame_bytes.saturating_mul(fps.max(1)).saturating_mul(target_seconds);
    needed.clamp(
        VIDEOENC_QUEUE_BYTES_FLOOR as u64,
        VIDEOENC_QUEUE_BYTES_CEILING as u64,
    ) as u32
}

/// Computes the target (width, height) for downscaling. Returns `None` when
/// scaling shouldn't happen — either no target was requested, the source size
/// is unknown, or the requested height isn't smaller than the source.
fn composited_target_size(streams: &[Stream], target_height: u32) -> Option<(i32, i32)> {
    if target_height == 0 {
        return None;
    }
    let target_h = target_height as i32;

    let (src_w, src_h) = match streams {
        [single] => single.size()?,
        many if !many.is_empty() => {
            let mut total_w = 0i32;
            let mut max_h = 0i32;
            for s in many {
                let (w, h) = s.size()?;
                total_w += w;
                max_h = max_h.max(h);
            }
            (total_w, max_h)
        }
        _ => return None,
    };

    if target_h >= src_h {
        return None;
    }

    let scale = target_h as f64 / src_h as f64;
    let target_w = (src_w as f64 * scale).round() as i32 & !1;
    Some((target_w.max(2), target_h))
}

/// Creates a new audio src element with the given name.
///
/// If the class is already a source, it will return the device name as is,
/// otherwise, if it is a sink, it will append `.monitor` to the device name.
fn make_pulsesrc(class: DeviceClass, element_name: &str) -> Result<gst::Element> {
    let device = device::find_default(class)?;

    let pulsesrc = gst::ElementFactory::make("pulsesrc")
        .name(element_name)
        .property("provide-clock", false)
        .property("do-timestamp", true)
        .build()?;

    match class {
        DeviceClass::Sink => {
            let pulsesink = device.create_element(None)?;
            let device_name = pulsesink
                .property::<Option<String>>("device")
                .context("No device name")?;
            ensure!(!device_name.is_empty(), "Empty device name");

            let monitor_name = format!("{}.monitor", device_name);
            pulsesrc.set_property("device", &monitor_name);

            tracing::debug!("Found desktop audio with name `{}`", monitor_name);
        }
        DeviceClass::Source => {
            device.reconfigure_element(&pulsesrc)?;

            let device_name = pulsesrc
                .property::<Option<String>>("device")
                .context("No device name")?;
            ensure!(!device_name.is_empty(), "Empty device name");

            tracing::debug!("Found microphone with name `{}`", device_name);
        }
    }

    Ok(pulsesrc)
}

/// Creates a bin with a src pad for a pulse audio device
///
/// pulsesrc1 -> audiorate -> |
///                           |
/// pulsesrc2 -> audiorate -> | -> audiomixer
///                           |
/// pulsesrcn -> audiorate -> |
fn make_audiosrc_bin<'a>(
    pulsesrcs: impl IntoIterator<Item = &'a gst::Element>,
) -> Result<gst::Bin> {
    let bin = gst::Bin::builder().name("kooha-pulsesrc-bin").build();

    let caps = gst::Caps::builder_full()
        .structure(
            gst::Structure::builder("audio/x-raw")
                .field("rate", AUDIO_SAMPLE_RATE)
                .field("channels", 2)
                .build(),
        )
        .structure(
            gst::Structure::builder("audio/x-raw")
                .field("rate", AUDIO_SAMPLE_RATE)
                .field("channels", 1)
                .build(),
        )
        .build();

    let audiomixer = gst::ElementFactory::make("audiomixer")
        .property("latency", gst::ClockTime::from_seconds(2))
        .build()?;
    let audiomixer_capsfilter = gst::ElementFactory::make("capsfilter")
        .property("caps", &caps)
        .build()?;
    bin.add_many([&audiomixer, &audiomixer_capsfilter])?;
    audiomixer.link(&audiomixer_capsfilter)?;

    let src_pad = audiomixer_capsfilter.static_pad("src").unwrap();
    bin.add_pad(&gst::GhostPad::with_target(&src_pad)?)?;

    for pulsesrc in pulsesrcs {
        let audiorate = gst::ElementFactory::make("audiorate")
            .property("skip-to-first", true)
            .build()?;

        bin.add_many([pulsesrc, &audiorate])?;
        pulsesrc.link_filtered(&audiorate, &caps)?;
        audiorate.link_pads(None, &audiomixer, Some("sink_%u"))?;
    }

    Ok(bin)
}

fn round_to_even(number: i32) -> i32 {
    number / 2 * 2
}

fn round_to_even_f32(number: f32) -> i32 {
    (number / 2.0).round() as i32 * 2
}

#[cfg(test)]
mod test {
    use super::*;

    macro_rules! assert_even {
        ($number:expr) => {
            assert_eq!($number % 2, 0)
        };
    }

    #[test]
    fn odd_round_to_even() {
        assert_even!(round_to_even(5));
        assert_even!(round_to_even(101));
    }

    #[test]
    fn odd_round_to_even_f32() {
        assert_even!(round_to_even_f32(3.0));
        assert_even!(round_to_even_f32(99.0));
    }

    #[test]
    fn even_round_to_even() {
        assert_even!(round_to_even(50));
        assert_even!(round_to_even(4));
    }

    #[test]
    fn even_round_to_even_f32() {
        assert_even!(round_to_even_f32(300.0));
        assert_even!(round_to_even_f32(6.0));
    }

    #[test]
    fn float_round_to_even_f32() {
        assert_even!(round_to_even_f32(5.3));
        assert_even!(round_to_even_f32(2.9));
    }
}
