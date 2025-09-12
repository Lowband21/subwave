pub mod gst_play_flags {
    use gstreamer::glib::{
        Type, Value, bitflags, gobject_ffi, prelude::*, translate::*, value::FromValue,
    };
    use std::fmt;

    bitflags::bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct GstPlayFlags: u32 {
            /// Render the video stream
            const VIDEO             = 0x00000001;
            /// Render the audio stream
            const AUDIO             = 0x00000002;
            /// Render subtitles
            const TEXT              = 0x00000004;
            /// Render visualisation when no video is present
            const VIS               = 0x00000008;
            /// Use software volume
            const SOFT_VOLUME       = 0x00000010;
            /// Only use native audio formats
            const NATIVE_AUDIO      = 0x00000020;
            /// Only use native video formats
            const NATIVE_VIDEO      = 0x00000040;
            /// Attempt progressive download buffering
            const DOWNLOAD          = 0x00000080;
            /// Buffer demuxed/parsed data
            const BUFFERING         = 0x00000100;
            /// Deinterlace video if necessary
            const DEINTERLACE       = 0x00000200;
            /// Use software color balance
            const SOFT_COLORBALANCE = 0x00000400;
            /// Force audio/video filter(s) to be applied
            const FORCE_FILTERS     = 0x00000800;
            /// Force only software-based decoders (no effect for playbin3)
            const FORCE_SW_DECODERS = 0x00001000;
        }
    }

    impl Default for GstPlayFlags {
        fn default() -> Self {
            // Default flags for typical playback
            GstPlayFlags::VIDEO
                | GstPlayFlags::AUDIO
                | GstPlayFlags::TEXT
                | GstPlayFlags::SOFT_VOLUME
        }
    }

    impl fmt::Display for GstPlayFlags {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let mut flags = Vec::new();
            if self.contains(Self::VIDEO) {
                flags.push("VIDEO");
            }
            if self.contains(Self::AUDIO) {
                flags.push("AUDIO");
            }
            if self.contains(Self::TEXT) {
                flags.push("TEXT");
            }
            if self.contains(Self::VIS) {
                flags.push("VIS");
            }
            if self.contains(Self::SOFT_VOLUME) {
                flags.push("SOFT_VOLUME");
            }
            if self.contains(Self::NATIVE_AUDIO) {
                flags.push("NATIVE_AUDIO");
            }
            if self.contains(Self::NATIVE_VIDEO) {
                flags.push("NATIVE_VIDEO");
            }
            if self.contains(Self::DOWNLOAD) {
                flags.push("DOWNLOAD");
            }
            if self.contains(Self::BUFFERING) {
                flags.push("BUFFERING");
            }
            if self.contains(Self::DEINTERLACE) {
                flags.push("DEINTERLACE");
            }
            if self.contains(Self::SOFT_COLORBALANCE) {
                flags.push("SOFT_COLORBALANCE");
            }
            if self.contains(Self::FORCE_FILTERS) {
                flags.push("FORCE_FILTERS");
            }
            if self.contains(Self::FORCE_SW_DECODERS) {
                flags.push("FORCE_SW_DECODERS");
            }
            write!(f, "GstPlayFlags({})", flags.join(" | "))
        }
    }

    impl StaticType for GstPlayFlags {
        fn static_type() -> Type {
            // GStreamer registers this type internally
            Type::from_name("GstPlayFlags")
                .expect("GstPlayFlags type should be registered by GStreamer")
        }
    }

    impl ToValue for GstPlayFlags {
        fn to_value(&self) -> Value {
            unsafe {
                let mut value = Value::from_type(Self::static_type());
                gobject_ffi::g_value_set_flags(value.to_glib_none_mut().0, self.bits());
                value
            }
        }

        fn value_type(&self) -> Type {
            Self::static_type()
        }
    }

    impl From<GstPlayFlags> for Value {
        fn from(flags: GstPlayFlags) -> Self {
            flags.to_value()
        }
    }

    unsafe impl<'a> FromValue<'a> for GstPlayFlags {
        type Checker = gstreamer::glib::value::GenericValueTypeChecker<Self>;

        unsafe fn from_value(value: &'a Value) -> Self {
            unsafe {
                let bits = gobject_ffi::g_value_get_flags(value.to_glib_none().0);
                GstPlayFlags::from_bits_truncate(bits)
            }
        }
    }

    impl GstPlayFlags {
        /// Get the default flags for network streaming with download enabled
        pub fn for_network_stream() -> Self {
            Self::VIDEO
                | Self::AUDIO
                | Self::TEXT
                | Self::SOFT_VOLUME
                | Self::BUFFERING
                | Self::DEINTERLACE
        }

        /// Get minimal flags for audio-only playback
        pub fn audio_only() -> Self {
            Self::AUDIO | Self::SOFT_VOLUME
        }

        /// Get flags for video playback without text
        pub fn video_no_text() -> Self {
            Self::VIDEO | Self::AUDIO | Self::SOFT_VOLUME
        }
    }
}
