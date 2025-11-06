use gstreamer as gst;
use gstreamer::prelude::*;

/// Build a GStreamer `Context` of type `"http-headers"` from provided headers.
/// Returns `None` if the provided slice is empty.
pub fn build_http_headers_context<T: AsRef<str>, U: AsRef<str>>(
    headers: &[(T, U)],
) -> Option<gst::Context> {
    if headers.is_empty() {
        return None;
    }
    let mut ctx = gst::Context::new("http-headers", true);
    {
        let s = ctx.get_mut().unwrap().structure_mut();
        for (k, v) in headers.iter() {
            s.set(k.as_ref(), &v.as_ref());
        }
    }
    Some(ctx)
}

/// Convenience helper to apply HTTP headers to a pipeline using the `http-headers` context.
/// Returns true if a context was applied.
pub fn set_http_headers_on_pipeline<T: AsRef<str>, U: AsRef<str>>(
    pipeline: &gst::Pipeline,
    headers: &[(T, U)],
) -> bool {
    if let Some(ctx) = build_http_headers_context(headers) {
        pipeline.set_context(&ctx);
        true
    } else {
        false
    }
}
