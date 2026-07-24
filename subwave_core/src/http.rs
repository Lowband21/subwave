use std::sync::Arc;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Build the structure expected by HTTP sources such as `souphttpsrc`.
pub fn build_extra_headers<T: AsRef<str>, U: AsRef<str>>(
    headers: &[(T, U)],
) -> Option<gst::Structure> {
    if headers.is_empty() {
        return None;
    }

    let mut extra_headers = gst::Structure::new_empty("extra-headers");
    for (name, value) in headers {
        extra_headers.set(name.as_ref(), value.as_ref());
    }
    Some(extra_headers)
}

fn set_headers_on_source(source: &gst::Element, headers: &[(String, String)]) -> bool {
    if !source.has_property("extra-headers") {
        return false;
    }

    let Some(extra_headers) = build_extra_headers(headers) else {
        return false;
    };
    source.set_property("extra-headers", extra_headers);
    true
}

/// Apply HTTP headers to current and future HTTP sources in a playback pipeline.
///
/// `souphttpsrc` consumes headers through its `extra-headers` property, not a
/// `GstContext`. The `source-setup` hook configures the primary URI source,
/// while `deep-element-added` covers sources created in nested bins.
pub fn set_http_headers_on_pipeline<T: AsRef<str>, U: AsRef<str>>(
    pipeline: &gst::Pipeline,
    headers: &[(T, U)],
) -> bool {
    if headers.is_empty() {
        return false;
    }

    let headers = Arc::new(
        headers
            .iter()
            .map(|(name, value)| (name.as_ref().to_string(), value.as_ref().to_string()))
            .collect::<Vec<_>>(),
    );

    if gst::glib::subclass::SignalId::lookup("source-setup", pipeline.type_()).is_some() {
        let source_headers = Arc::clone(&headers);
        pipeline.connect("source-setup", false, move |values| {
            if let Some(source) = values
                .get(1)
                .and_then(|value| value.get::<gst::Element>().ok())
            {
                set_headers_on_source(&source, source_headers.as_slice());
            }
            None
        });
    }

    let nested_headers = Arc::clone(&headers);
    pipeline.connect_deep_element_added(move |_pipeline, _bin, element| {
        set_headers_on_source(element, nested_headers.as_slice());
    });

    for element in pipeline.iterate_recurse().into_iter().flatten() {
        set_headers_on_source(&element, headers.as_slice());
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_headers(source: &gst::Element) -> gst::Structure {
        source
            .property::<Option<gst::Structure>>("extra-headers")
            .expect("HTTP source should have extra headers")
    }

    #[test]
    fn source_setup_applies_headers_to_primary_source() {
        gst::init().unwrap();
        let pipeline = gst::ElementFactory::make("playbin3")
            .build()
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let source = gst::ElementFactory::make("souphttpsrc").build().unwrap();

        assert!(set_http_headers_on_pipeline(
            &pipeline,
            &[("Authorization", "Bearer playback-ticket")],
        ));
        pipeline.emit_by_name::<()>("source-setup", &[&source]);

        assert_eq!(
            source_headers(&source).get::<String>("Authorization"),
            Ok("Bearer playback-ticket".to_string())
        );
    }

    #[test]
    fn nested_http_sources_receive_headers() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let bin = gst::Bin::new();
        pipeline.add(&bin).unwrap();

        assert!(set_http_headers_on_pipeline(
            &pipeline,
            &[("Authorization", "Bearer playback-ticket")],
        ));

        let source = gst::ElementFactory::make("souphttpsrc").build().unwrap();
        bin.add(&source).unwrap();

        assert_eq!(
            source_headers(&source).get::<String>("Authorization"),
            Ok("Bearer playback-ticket".to_string())
        );
    }

    #[test]
    fn empty_headers_are_not_installed() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();

        assert!(!set_http_headers_on_pipeline::<&str, &str>(&pipeline, &[],));
    }
}
