use iced::{
    widget::{button, column, container, row, text},
    Element, Length, Task, Theme,
};
use iced_video_player_wayland::{Video, VideoPlayer};
use std::sync::{Arc, Mutex};

fn main() -> iced::Result {
    // Initialize GStreamer
    iced_video_player_wayland::init().expect("Failed to initialize GStreamer");

    // Force wgpu renderer
    iced::application(init, update, view)
        .theme(|_| Theme::default())
        .window(iced::window::Settings {
            size: iced::Size::new(1280.0, 720.0),
            resizable: true,
            decorations: true,
            ..Default::default()
        })
        .run()
}

struct State {
    video: Arc<Mutex<Option<Box<Video>>>>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
enum Message {
    PlayPause,
    Stop,
}

fn init() -> (State, Task<Message>) {
    // Try to load a video from command line args or use a default
    let video_path = if let Some(path) = std::env::args().nth(1) {
        path
    } else {
        String::from("http://localhost:3000/stream/956acde3-1e69-47d3-9a09-2093e485c220")
    };

    if let Ok(uri) = url::Url::parse(video_path.as_str()) {
        // Try to load video synchronously
        let (video, error) = match Video::new(&uri) {
            Ok(video) => (Some(Box::new(video)), None),
            Err(e) => (None, Some(format!("Failed to create video: {}", e))),
        };

        log::info!("Error: {:?}", error);

        let state = State {
            video: Arc::new(Mutex::new(video)),
            error,
        };
        (state, Task::none())
    } else {
        panic!("Invalid video path");
    }
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::PlayPause => {
            if let Ok(guard) = state.video.lock() {
                if let Some(video) = guard.as_ref() {
                    if let Err(e) = video.toggle_play() {
                        state.error = Some(format!("Playback error: {}", e));
                    }
                }
            }
        }
        Message::Stop => {
            if let Ok(guard) = state.video.lock() {
                if let Some(video) = guard.as_ref() {
                    if let Err(e) = video.stop() {
                        state.error = Some(format!("Stop error: {}", e));
                    }
                }
            }
        }
    }

    Task::none()
}

fn view(state: &State) -> Element<'_, Message, Theme, iced_wgpu::Renderer> {
    let has_video = state.video.lock().map(|g| g.is_some()).unwrap_or(false);

    let content = if has_video {
        column![
            VideoPlayer::new(&state.video)
                .width(Length::Fill)
                .height(Length::FillPortion(9))
                .content_fit(iced::ContentFit::Cover),
            container(
                row![
                    button("Play/Pause").on_press(Message::PlayPause),
                    button("Stop").on_press(Message::Stop),
                ]
                .spacing(10)
            )
            .width(Length::Fill)
            .height(Length::FillPortion(1))
            .center_x(Length::Fill)
            .center_y(Length::Fill),
        ]
    } else if let Some(error) = &state.error {
        column![
            text(format!("Error: {}", error)).size(20),
            text("Usage: cargo run --example simple_player <video_file>").size(14),
        ]
    } else {
        column![
            text("No video loaded").size(20),
            text("Usage: cargo run --example simple_player <video_file>").size(14),
        ]
    };

    let root: Element<'_, Message, Theme, iced_wgpu::Renderer> = container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into();

    root
}
