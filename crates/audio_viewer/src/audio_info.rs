use gpui::{Context, Entity, IntoElement, ParentElement, Render, Subscription, div};
use project::audio_store::AudioMetadata;
use ui::prelude::*;
use util::size::format_file_size;
use workspace::{ItemHandle, StatusItemView, Workspace};

use crate::AudioView;

pub struct AudioInfo {
    metadata: Option<AudioMetadata>,
    _observe_active_audio: Option<Subscription>,
    observe_audio_item: Option<Subscription>,
}

impl AudioInfo {
    pub fn new(_workspace: &Workspace) -> Self {
        Self {
            metadata: None,
            _observe_active_audio: None,
            observe_audio_item: None,
        }
    }

    fn update_metadata(&mut self, audio_view: &Entity<AudioView>, cx: &mut Context<Self>) {
        let audio_item = audio_view.read(cx).audio_item.clone();
        let current_metadata = audio_item.read(cx).audio_metadata;
        if current_metadata.is_some() {
            self.metadata = current_metadata;
            cx.notify();
        } else {
            self.observe_audio_item = Some(cx.observe(&audio_item, |this, item, cx| {
                this.metadata = item.read(cx).audio_metadata;
                cx.notify();
            }));
        }
    }
}

fn format_duration(duration: &std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{}:{:02}", minutes, seconds)
}

impl Render for AudioInfo {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(metadata) = self.metadata.as_ref() else {
            return div().hidden();
        };

        let mut components = Vec::new();

        if let Some(duration) = &metadata.duration {
            components.push(format_duration(duration));
        }

        if metadata.sample_rate > 0 {
            let sample_rate_khz = metadata.sample_rate as f64 / 1000.0;
            if sample_rate_khz == sample_rate_khz.floor() {
                components.push(format!("{}kHz", sample_rate_khz as u32));
            } else {
                components.push(format!("{:.1}kHz", sample_rate_khz));
            }
        }

        match metadata.channels {
            1 => components.push("Mono".to_string()),
            2 => components.push("Stereo".to_string()),
            count => components.push(format!("{} channels", count)),
        }

        if let Some(bitrate) = metadata.bitrate {
            components.push(format!("{}kbps", bitrate));
        }

        components.push(format_file_size(metadata.file_size, false));
        components.push(metadata.format.to_string());

        div().child(Label::new(components.join(" • ")).size(LabelSize::Small))
    }
}

impl StatusItemView for AudioInfo {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._observe_active_audio = None;
        self.observe_audio_item = None;

        if let Some(audio_view) = active_pane_item.and_then(|item| item.act_as::<AudioView>(cx)) {
            self.update_metadata(&audio_view, cx);

            self._observe_active_audio = Some(cx.observe(&audio_view, |this, view, cx| {
                this.update_metadata(&view, cx);
            }));
        } else {
            self.metadata = None;
        }
        cx.notify();
    }
}
