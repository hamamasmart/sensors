//! Capture a single frame from an RTSP stream and return it as PNG bytes.
//!
//! We open the stream with `ffmpeg_next`, decode the first video packet, and
//! scale the frame to packed RGB24 via ffmpeg's swscale. The RGB24 pixels are
//! then encoded to PNG with the `image` crate — ffmpeg is used only for
//! decoding/scaling, never for PNG encoding, so we don't depend on ffmpeg's
//! PNG codec being built (the `build` feature's `--disable-autodetect` drops
//! zlib and therefore the PNG encoder). The server stores every snapshot
//! under a `.png` key, so the output is always a real PNG regardless of what
//! the camera streams (typically H.264).
//!
//! RTSP runs over TCP with a socket read timeout so a dead/unresponsive camera
//! fails fast instead of stalling the capture loop.

use anyhow::Context;
use ffmpeg_next as ffmpeg;

use ffmpeg::codec::Context as CodecContext;
use ffmpeg::format::{Pixel, input_with_dictionary};
use ffmpeg::media::Type;
use ffmpeg::software::scaling::Context as Scaler;
use ffmpeg::util::frame::video::Video;
use ffmpeg::Dictionary;

/// Upper bound on how long we'll wait for the stream to produce a frame, in
/// microseconds. Applied as the RTSP socket read timeout (`stimeout`).
const SOCKET_TIMEOUT_US: i64 = 5_000_000;

/// Capture the first available frame from `rtsp_url` and return it as PNG bytes.
pub fn capture_frame_png(rtsp_url: &str) -> anyhow::Result<Vec<u8>> {
    // Safe to call repeatedly — `avformat_network_init` is refcounted.
    ffmpeg::init()?;

    let mut opts = Dictionary::new();
    opts.set("rtsp_transport", "tcp");
    opts.set("stimeout", &SOCKET_TIMEOUT_US.to_string());

    let mut ictx = input_with_dictionary(rtsp_url, opts)
        .with_context(|| format!("failed to open RTSP stream `{rtsp_url}`"))?;

    let stream = ictx
        .streams()
        .best(Type::Video)
        .context("no video stream found in RTSP input")?;
    let stream_index = stream.index();

    let mut decoder = CodecContext::from_parameters(stream.parameters())?
        .decoder()
        .video()?;

    // Decode packets until we have one frame, then re-encode it as PNG. The
    // scaler is built from the decoded frame (not the decoder context) because
    // some H.264 streams only report a pixel format after the first frame is
    // decoded — building it up front would see `Pixel::None` and fail.
    let mut scaler: Option<Scaler> = None;
    let mut decoded = Video::empty();
    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        // `receive_frame` may need several packets before producing a frame,
        // so retry on the next packet rather than bailing on EAGAIN.
        if decoder.receive_frame(&mut decoded).is_ok() {
            return scale_and_encode(&decoded, &mut scaler);
        }
    }

    // Flush the decoder in case the first frame was buffered.
    decoder.send_eof()?;
    if decoder.receive_frame(&mut decoded).is_ok() {
        return scale_and_encode(&decoded, &mut scaler);
    }

    anyhow::bail!("RTSP stream `{rtsp_url}` produced no decodable frame");
}

/// Scale `decoded` to packed RGB24 (building the scaler lazily on first use)
/// and encode the result as PNG.
fn scale_and_encode(decoded: &Video, scaler: &mut Option<Scaler>) -> anyhow::Result<Vec<u8>> {
    if scaler.is_none() {
        *scaler = Some(
            decoded
                .converter(Pixel::RGB24)
                .context("creating RGB scaler failed")?,
        );
    }
    let scaler = scaler.as_mut().expect("scaler initialized above");

    let mut rgb = Video::empty();
    scaler
        .run(decoded, &mut rgb)
        .context("scaling frame to RGB24 failed")?;
    encode_png(&rgb)
}

/// Encode an RGB24 frame to a complete PNG file's bytes via the `image` crate.
///
/// ffmpeg's frame data is row-padded: each row starts at `stride(0)` bytes
/// apart, which can exceed `width * 3`. `image`'s `RgbImage` expects tightly
/// packed rows, so we copy each row stripping the trailing padding before
/// handing the buffer to the PNG encoder.
fn encode_png(rgb: &Video) -> anyhow::Result<Vec<u8>> {
    let width = rgb.width();
    let height = rgb.height();
    let stride = rgb.stride(0);
    let src = rgb.data(0);
    let row_bytes = usize::try_from(width).unwrap() * 3;

    let mut packed = Vec::with_capacity(row_bytes * usize::try_from(height).unwrap());
    for y in 0..usize::try_from(height).unwrap() {
        let row_start = y * stride;
        packed.extend_from_slice(&src[row_start..row_start + row_bytes]);
    }

    let img = image::RgbImage::from_raw(width, height, packed)
        .context("RGB24 frame dimensions don't match its byte count")?;

    let mut out = Vec::new();
    img.write_with_encoder(image::codecs::png::PngEncoder::new(&mut out))
        .context("PNG encode failed")?;
    Ok(out)
}
