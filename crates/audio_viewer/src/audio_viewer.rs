mod audio_info;

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use editor::{EditorSettings, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Task, WeakEntity, Window, actions,
    canvas, div, point, px, size,
};
use language::File as _;
use persistence::AUDIO_VIEWER;
use project::{AudioItem, Project, ProjectPath, audio_store::AudioItemEvent};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Source as _, mixer::Mixer};
use settings::Settings;
use theme::ThemeSettings;
use ui::{Tooltip, prelude::*};
use util::paths::PathExt;
use workspace::{
    ItemId, ItemSettings, Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
    WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{BreadcrumbText, Item, ItemHandle, ProjectItem, SerializableItem, TabContentParams},
};

pub use crate::audio_info::*;

actions!(
    audio_viewer,
    [
        /// Toggle play/pause.
        TogglePlayback,
    ]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

pub struct AudioView {
    audio_item: Entity<AudioItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    playback_state: PlaybackState,
    current_position: Duration,
    duration: Option<Duration>,
    output_handle: Option<OutputStream>,
    mixer: Option<Mixer>,
    _position_update_task: Option<Task<()>>,
}

impl AudioView {
    pub fn new(
        audio_item: Entity<AudioItem>,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&audio_item, Self::on_audio_event).detach();

        let duration = audio_item
            .read(cx)
            .audio_metadata
            .as_ref()
            .and_then(|metadata| metadata.duration);

        Self {
            audio_item,
            project,
            focus_handle: cx.focus_handle(),
            playback_state: PlaybackState::Stopped,
            current_position: Duration::ZERO,
            duration,
            output_handle: None,
            mixer: None,
            _position_update_task: None,
        }
    }

    fn on_audio_event(
        &mut self,
        _: Entity<AudioItem>,
        event: &AudioItemEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            AudioItemEvent::MetadataUpdated | AudioItemEvent::FileHandleChanged => {
                self.duration = self
                    .audio_item
                    .read(cx)
                    .audio_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.duration);
                cx.emit(AudioViewEvent::TitleChanged);
                cx.notify();
            }
            AudioItemEvent::Reloaded => {
                self.stop_playback(cx);
                cx.emit(AudioViewEvent::TitleChanged);
                cx.notify();
            }
            AudioItemEvent::ReloadNeeded => {}
        }
    }

    fn toggle_playback(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        match self.playback_state {
            PlaybackState::Playing => self.pause_playback(cx),
            PlaybackState::Paused | PlaybackState::Stopped => self.start_playback(cx),
        }
    }

    fn start_playback(&mut self, cx: &mut Context<Self>) {
        let audio_bytes = self.audio_item.read(cx).audio_bytes.clone();
        let skip = self.current_position;

        match self.try_start_playback(audio_bytes, skip) {
            Ok(()) => {
                self.playback_state = PlaybackState::Playing;
                self.start_position_tracking(cx);
                cx.notify();
            }
            Err(error) => {
                log::error!("Failed to start audio playback: {error:#}");
            }
        }
    }

    fn try_start_playback(
        &mut self,
        audio_bytes: Arc<Vec<u8>>,
        skip: Duration,
    ) -> anyhow::Result<()> {
        let mut output_handle =
            OutputStreamBuilder::open_default_stream().context("Could not open audio output")?;
        output_handle.log_on_drop(false);

        let (mixer, mixer_source) = rodio::mixer::mixer(rodio::nz!(2), rodio::nz!(44100));
        mixer.add(rodio::source::Zero::new(rodio::nz!(2), rodio::nz!(44100)));

        let cursor = Cursor::new((*audio_bytes).clone());
        let mut source = Decoder::new(cursor).context("Could not decode audio")?;
        if skip > Duration::ZERO {
            source
                .try_seek(skip)
                .context("Could not seek audio source")?;
        }
        mixer.add(source);

        output_handle.mixer().add(mixer_source);

        self.output_handle = Some(output_handle);
        self.mixer = Some(mixer);

        Ok(())
    }

    fn start_position_tracking(&mut self, cx: &mut Context<Self>) {
        let start_time = std::time::Instant::now();
        let start_position = self.current_position;

        self._position_update_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;

                let should_continue = this
                    .update(cx, |this, cx| {
                        if this.playback_state != PlaybackState::Playing {
                            return false;
                        }

                        let elapsed = start_time.elapsed();
                        this.current_position = start_position + elapsed;

                        if let Some(duration) = this.duration {
                            if this.current_position >= duration {
                                this.current_position = duration;
                                this.playback_state = PlaybackState::Stopped;
                                this.current_position = Duration::ZERO;
                                this.output_handle = None;
                                this.mixer = None;
                                this._position_update_task = None;
                                cx.notify();
                                return false;
                            }
                        }

                        cx.notify();
                        true
                    })
                    .unwrap_or(false);

                if !should_continue {
                    break;
                }
            }
        }));
    }

    fn pause_playback(&mut self, cx: &mut Context<Self>) {
        self.playback_state = PlaybackState::Paused;
        self._position_update_task = None;
        self.output_handle = None;
        self.mixer = None;
        cx.notify();
    }

    fn stop_playback(&mut self, cx: &mut Context<Self>) {
        self.playback_state = PlaybackState::Stopped;
        self.current_position = Duration::ZERO;
        self._position_update_task = None;
        self.output_handle = None;
        self.mixer = None;
        cx.notify();
    }

    fn toggle_playback_action(
        &mut self,
        _: &TogglePlayback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_playback(window, cx);
    }

    fn seek_to_fraction(&mut self, fraction: f32, cx: &mut Context<Self>) {
        let Some(duration) = self.duration else {
            return;
        };

        let target =
            Duration::from_secs_f64(duration.as_secs_f64() * fraction.clamp(0.0, 1.0) as f64);
        self.current_position = target;

        if self.playback_state == PlaybackState::Playing {
            self.output_handle = None;
            self.mixer = None;
            self._position_update_task = None;
            self.start_playback(cx);
        } else {
            cx.notify();
        }
    }

    fn format_time_display(&self) -> String {
        let position = format_duration(self.current_position);
        if let Some(duration) = self.duration {
            format!("{} / {}", position, format_duration(duration))
        } else {
            position
        }
    }

    fn playback_fraction(&self) -> f32 {
        if let Some(duration) = self.duration {
            if duration.as_secs_f64() > 0.0 {
                return (self.current_position.as_secs_f64() / duration.as_secs_f64()) as f32;
            }
        }
        0.0
    }
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{}:{:02}", minutes, seconds)
}

pub enum AudioViewEvent {
    TitleChanged,
}

impl EventEmitter<AudioViewEvent> for AudioView {}

impl Item for AudioView {
    type Event = AudioViewEvent;

    fn to_item_events(event: &Self::Event, mut f: impl FnMut(workspace::item::ItemEvent)) {
        match event {
            AudioViewEvent::TitleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
                f(workspace::item::ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.audio_item.entity_id(), self.audio_item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.audio_item.read(cx).abs_path(cx)?;
        let file_path = abs_path.compact().to_string_lossy().into_owned();
        Some(file_path.into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let project_path = self.audio_item.read(cx).project_path(cx);

        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .project_path_git_status(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();

            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    entry_git_aware_label_color(git_status, entry.is_ignored, params.selected)
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.audio_item
            .read(cx)
            .file
            .file_name(cx)
            .to_string()
            .into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.audio_item.read(cx).abs_path(cx)?;
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        let show_breadcrumb = EditorSettings::get_global(cx).toolbar.breadcrumbs;
        if show_breadcrumb {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<Vec<BreadcrumbText>> {
        let text = breadcrumbs_text_for_audio(self.project.read(cx), self.audio_item.read(cx), cx);
        let settings = ThemeSettings::get_global(cx);

        Some(vec![BreadcrumbText {
            text,
            highlights: None,
            font: Some(settings.buffer_font.clone()),
        }])
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| Self {
            audio_item: self.audio_item.clone(),
            project: self.project.clone(),
            focus_handle: cx.focus_handle(),
            playback_state: PlaybackState::Stopped,
            current_position: Duration::ZERO,
            duration: self.duration,
            output_handle: None,
            mixer: None,
            _position_update_task: None,
        })))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.audio_item.read(cx).file.disk_state().is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

fn breadcrumbs_text_for_audio(project: &Project, audio: &AudioItem, cx: &App) -> String {
    let mut path = audio.file.path().clone();
    if project.visible_worktrees(cx).count() > 1
        && let Some(worktree) = project.worktree_for_id(audio.project_path(cx).worktree_id, cx)
    {
        path = worktree.read(cx).root_name().join(&path);
    }

    path.display(project.path_style(cx)).to_string()
}

impl EventEmitter<()> for AudioView {}

impl Focusable for AudioView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AudioView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_playing = matches!(self.playback_state, PlaybackState::Playing);
        let fraction = self.playback_fraction();
        let time_display = self.format_time_display();
        let track_color = cx.theme().colors().border;
        let fill_color = cx.theme().colors().icon_accent;
        let entity = cx.entity();

        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("AudioViewer")
            .on_action(cx.listener(Self::toggle_playback_action))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                Icon::new(IconName::AudioOn)
                    .size(IconSize::XLarge)
                    .color(Color::Muted),
            )
            .child(Label::new(time_display).size(LabelSize::Large))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        IconButton::new(
                            "play-pause",
                            if is_playing {
                                IconName::DebugPause
                            } else {
                                IconName::PlayFilled
                            },
                        )
                        .icon_size(IconSize::Medium)
                        .tooltip(move |_window, cx| {
                            Tooltip::for_action(
                                if is_playing { "Pause" } else { "Play" },
                                &TogglePlayback,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_playback(window, cx);
                        })),
                    )
                    .child(
                        div()
                            .w(px(240.0))
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .cursor_pointer()
                            .child(
                                canvas(move |bounds, _window, _cx| bounds, {
                                    let entity = entity;
                                    move |bounds, _, window, _cx| {
                                        window.on_mouse_event({
                                            let entity = entity.clone();
                                            move |event: &MouseDownEvent, _phase, _window, cx| {
                                                if event.button != MouseButton::Left {
                                                    return;
                                                }
                                                if !bounds.contains(&event.position) {
                                                    return;
                                                }
                                                let click_x: f32 =
                                                    (event.position.x - bounds.origin.x).into();
                                                let width: f32 = bounds.size.width.into();
                                                if width > 0.0 {
                                                    entity.update(cx, |this, cx| {
                                                        this.seek_to_fraction(click_x / width, cx);
                                                    });
                                                }
                                            }
                                        });
                                        let origin_x: f32 = bounds.origin.x.into();
                                        let origin_y: f32 = bounds.origin.y.into();
                                        let width: f32 = bounds.size.width.into();
                                        let height: f32 = bounds.size.height.into();

                                        window.paint_quad(gpui::fill(
                                            gpui::Bounds::new(
                                                point(px(origin_x), px(origin_y)),
                                                size(px(width), px(height)),
                                            ),
                                            track_color,
                                        ));

                                        let fill_width = width * fraction;
                                        if fill_width > 0.0 {
                                            window.paint_quad(gpui::fill(
                                                gpui::Bounds::new(
                                                    point(px(origin_x), px(origin_y)),
                                                    size(px(fill_width), px(height)),
                                                ),
                                                fill_color,
                                            ));
                                        }
                                    }
                                })
                                .size_full(),
                            ),
                    ),
            )
    }
}

impl ProjectItem for AudioView {
    type Item = AudioItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView>
    where
        Self: Sized,
    {
        Some(InvalidItemView::new(abs_path, is_local, error, window, cx))
    }
}

impl SerializableItem for AudioView {
    fn serialized_item_kind() -> &'static str {
        "AudioView"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let audio_path = AUDIO_VIEWER
                .get_audio_path(item_id, workspace_id)?
                .context("No audio path found")?;

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(audio_path.clone(), false, cx)
                })
                .await
                .context("Path not found")?;
            let worktree_id = worktree.update(cx, |worktree, _cx| worktree.id());

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let audio_item = project
                .update(cx, |project, cx| project.open_audio(project_path, cx))
                .await?;

            cx.update(
                |window, cx| Ok(cx.new(|cx| AudioView::new(audio_item, project, window, cx))),
            )?
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        delete_unloaded_items(
            alive_items,
            workspace_id,
            "audio_viewers",
            &AUDIO_VIEWER,
            cx,
        )
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let audio_path = self.audio_item.read(cx).abs_path(cx)?;

        Some(cx.background_spawn({
            async move {
                log::debug!("Saving audio at path {audio_path:?}");
                AUDIO_VIEWER
                    .save_audio_path(item_id, workspace_id, audio_path)
                    .await
            }
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

pub struct AudioViewToolbarControls {
    audio_view: Option<WeakEntity<AudioView>>,
    _subscription: Option<gpui::Subscription>,
}

impl AudioViewToolbarControls {
    pub fn new() -> Self {
        Self {
            audio_view: None,
            _subscription: None,
        }
    }
}

impl Render for AudioViewToolbarControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(audio_view) = self.audio_view.as_ref().and_then(|v| v.upgrade()) else {
            return div().into_any_element();
        };

        let audio_view_state = audio_view.read(cx);
        let is_playing = matches!(audio_view_state.playback_state, PlaybackState::Playing);
        let time_display = audio_view_state.format_time_display();

        h_flex()
            .gap_1()
            .child(
                IconButton::new(
                    "play-pause",
                    if is_playing {
                        IconName::DebugPause
                    } else {
                        IconName::PlayFilled
                    },
                )
                .icon_size(IconSize::Small)
                .tooltip(move |_window, cx| {
                    Tooltip::for_action(
                        if is_playing { "Pause" } else { "Play" },
                        &TogglePlayback,
                        cx,
                    )
                })
                .on_click({
                    let audio_view = audio_view.downgrade();
                    move |_, window, cx| {
                        if let Some(view) = audio_view.upgrade() {
                            view.update(cx, |this, cx| {
                                this.toggle_playback(window, cx);
                            });
                        }
                    }
                }),
            )
            .child(
                Label::new(time_display)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
}

impl EventEmitter<ToolbarItemEvent> for AudioViewToolbarControls {}

impl ToolbarItemView for AudioViewToolbarControls {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.audio_view = None;
        self._subscription = None;

        if let Some(item) = active_pane_item.and_then(|i| i.downcast::<AudioView>()) {
            self._subscription = Some(cx.observe(&item, |_, _, cx| {
                cx.notify();
            }));
            self.audio_view = Some(item.downgrade());
            cx.notify();
            return ToolbarItemLocation::PrimaryRight;
        }

        ToolbarItemLocation::Hidden
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<AudioView>(cx);
    workspace::register_serializable_item::<AudioView>(cx);
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct AudioViewerDb(ThreadSafeConnection);

    impl Domain for AudioViewerDb {
        const NAME: &str = stringify!(AudioViewerDb);

        const MIGRATIONS: &[&str] = &[sql!(
                CREATE TABLE audio_viewers (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    audio_path BLOB,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
        )];
    }

    db::static_connection!(AUDIO_VIEWER, AudioViewerDb, [WorkspaceDb]);

    impl AudioViewerDb {
        query! {
            pub async fn save_audio_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                audio_path: PathBuf
            ) -> Result<()> {
                INSERT OR REPLACE INTO audio_viewers(item_id, workspace_id, audio_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_audio_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT audio_path
                FROM audio_viewers
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
