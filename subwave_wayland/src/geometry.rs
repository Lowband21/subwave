use iced::{ContentFit, Size};

/// Video destination relative to the widget's Wayland surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoRectangle {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

impl VideoRectangle {
    pub(crate) fn fill(width: i32, height: i32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }
}

/// Resolve an Iced content-fit mode into a Wayland render rectangle.
///
/// The rectangle is relative to the video widget. Modes are centered except
/// for Iced's native-size `None` mode, which remains anchored at the top-left.
/// `Cover` can intentionally produce negative offsets and dimensions larger
/// than the widget so the compositor clips the excess at the top-level surface
/// boundary.
pub(crate) fn fit_video_rectangle(
    content_fit: ContentFit,
    source_width: i32,
    source_height: i32,
    target_width: i32,
    target_height: i32,
) -> Option<VideoRectangle> {
    if source_width <= 0 || source_height <= 0 || target_width <= 0 || target_height <= 0 {
        return None;
    }

    let source = Size::new(source_width as f32, source_height as f32);
    let target = Size::new(target_width as f32, target_height as f32);
    let fitted = content_fit.fit(source, target);

    if !fitted.width.is_finite()
        || !fitted.height.is_finite()
        || fitted.width <= 0.0
        || fitted.height <= 0.0
        || fitted.width > i32::MAX as f32
        || fitted.height > i32::MAX as f32
    {
        return None;
    }

    let width = (fitted.width.round() as i32).max(1);
    let height = (fitted.height.round() as i32).max(1);
    let (x, y) = if content_fit == ContentFit::None {
        (0, 0)
    } else {
        (
            ((target_width - width) as f64 / 2.0).round() as i32,
            ((target_height - height) as f64 / 2.0).round() as i32,
        )
    };

    Some(VideoRectangle {
        x,
        y,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contain_upscales_1080p_to_4k() {
        assert_eq!(
            fit_video_rectangle(ContentFit::Contain, 1920, 1080, 3840, 2160),
            Some(VideoRectangle::fill(3840, 2160))
        );
    }

    #[test]
    fn contain_letterboxes_and_centers() {
        assert_eq!(
            fit_video_rectangle(ContentFit::Contain, 1440, 1080, 3840, 2160),
            Some(VideoRectangle {
                x: 480,
                y: 0,
                width: 2880,
                height: 2160,
            })
        );
    }

    #[test]
    fn cover_crops_and_centers() {
        assert_eq!(
            fit_video_rectangle(ContentFit::Cover, 1440, 1080, 3840, 2160),
            Some(VideoRectangle {
                x: 0,
                y: -360,
                width: 3840,
                height: 2880,
            })
        );
    }

    #[test]
    fn fill_stretches_to_target() {
        assert_eq!(
            fit_video_rectangle(ContentFit::Fill, 1440, 1080, 3840, 2160),
            Some(VideoRectangle::fill(3840, 2160))
        );
    }

    #[test]
    fn none_keeps_native_size_at_top_left() {
        assert_eq!(
            fit_video_rectangle(ContentFit::None, 1920, 1080, 3840, 2160),
            Some(VideoRectangle::fill(1920, 1080))
        );
    }

    #[test]
    fn scale_down_never_upscales() {
        assert_eq!(
            fit_video_rectangle(ContentFit::ScaleDown, 1920, 1080, 3840, 2160),
            Some(VideoRectangle {
                x: 960,
                y: 540,
                width: 1920,
                height: 1080,
            })
        );
        assert_eq!(
            fit_video_rectangle(ContentFit::ScaleDown, 3840, 2160, 1920, 1080),
            Some(VideoRectangle::fill(1920, 1080))
        );
    }

    #[test]
    fn rejects_non_positive_dimensions() {
        assert_eq!(
            fit_video_rectangle(ContentFit::Contain, 0, 1080, 3840, 2160),
            None
        );
        assert_eq!(
            fit_video_rectangle(ContentFit::Contain, 1920, 1080, 3840, 0),
            None
        );
    }
}
