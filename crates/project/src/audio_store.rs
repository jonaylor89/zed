use crate::{
    Project, ProjectEntryId, ProjectItem, ProjectPath,
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet, hash_map};
use futures::{StreamExt, channel::oneshot};
use gpui::{
    App, AsyncApp, Context, Entity, EventEmitter, Subscription, Task, WeakEntity, prelude::*,
};
use language::{DiskState, File};
use rpc::{AnyProtoClient, ErrorExt as _};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use util::{ResultExt, rel_path::RelPath};
use worktree::{LoadedBinaryFile, PathChange, Worktree};

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "wav", "ogg", "oga", "flac", "aac", "m4a", "opus", "wma", "webm",
];

#[derive(Clone, Copy, Debug, Hash, PartialEq, PartialOrd, Ord, Eq)]
pub struct AudioId(NonZeroU64);

impl AudioId {
    pub fn to_proto(&self) -> u64 {
        self.0.get()
    }
}

impl std::fmt::Display for AudioId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<NonZeroU64> for AudioId {
    fn from(id: NonZeroU64) -> Self {
        AudioId(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Mp3,
    Wav,
    Ogg,
    Flac,
    Aac,
    M4a,
    Unknown,
}

impl std::fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioFormat::Mp3 => write!(f, "MP3"),
            AudioFormat::Wav => write!(f, "WAV"),
            AudioFormat::Ogg => write!(f, "OGG"),
            AudioFormat::Flac => write!(f, "FLAC"),
            AudioFormat::Aac => write!(f, "AAC"),
            AudioFormat::M4a => write!(f, "M4A"),
            AudioFormat::Unknown => write!(f, "Unknown"),
        }
    }
}

impl AudioFormat {
    pub fn from_extension(extension: &str) -> Self {
        match extension.to_lowercase().as_str() {
            "mp3" => AudioFormat::Mp3,
            "wav" => AudioFormat::Wav,
            "ogg" | "oga" | "opus" => AudioFormat::Ogg,
            "flac" => AudioFormat::Flac,
            "aac" => AudioFormat::Aac,
            "m4a" => AudioFormat::M4a,
            _ => AudioFormat::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioMetadata {
    pub duration: Option<Duration>,
    pub sample_rate: u32,
    pub channels: u16,
    pub format: AudioFormat,
    pub file_size: u64,
    pub bitrate: Option<u32>,
}

#[derive(Debug)]
pub enum AudioItemEvent {
    ReloadNeeded,
    Reloaded,
    FileHandleChanged,
    MetadataUpdated,
}

impl EventEmitter<AudioItemEvent> for AudioItem {}

pub enum AudioStoreEvent {
    AudioAdded(Entity<AudioItem>),
}

impl EventEmitter<AudioStoreEvent> for AudioStore {}

pub struct AudioItem {
    pub id: AudioId,
    pub file: Arc<worktree::File>,
    pub audio_bytes: Arc<Vec<u8>>,
    reload_task: Option<Task<()>>,
    pub audio_metadata: Option<AudioMetadata>,
}

impl AudioItem {
    pub fn compute_metadata_from_bytes(
        audio_bytes: &[u8],
        extension: Option<&str>,
    ) -> Result<AudioMetadata> {
        let cursor = std::io::Cursor::new(audio_bytes.to_vec());
        let media_source_stream = MediaSourceStream::new(Box::new(cursor), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = extension {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe().format(
            &hint,
            media_source_stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;

        let format_reader = probed.format;
        let track = format_reader
            .default_track()
            .context("no audio track found")?;
        let codec_params = &track.codec_params;

        let sample_rate = codec_params.sample_rate.unwrap_or(0);
        let channels = codec_params
            .channels
            .map(|channel_layout| channel_layout.count() as u16)
            .unwrap_or(0);
        let duration = codec_params
            .n_frames
            .and_then(|frames| {
                let rate = codec_params.sample_rate?;
                if rate == 0 {
                    return None;
                }
                Some(Duration::from_secs_f64(frames as f64 / rate as f64))
            })
            .or_else(|| {
                codec_params.time_base.and_then(|time_base| {
                    codec_params.n_frames.map(|num_frames| {
                        let time = time_base.calc_time(num_frames);
                        Duration::from_secs_f64(time.seconds as f64 + time.frac)
                    })
                })
            });
        let bitrate = codec_params.bits_per_coded_sample;

        let audio_format = extension
            .map(AudioFormat::from_extension)
            .unwrap_or(AudioFormat::Unknown);

        Ok(AudioMetadata {
            duration,
            sample_rate,
            channels,
            format: audio_format,
            file_size: audio_bytes.len() as u64,
            bitrate,
        })
    }

    pub async fn load_audio_metadata(
        audio: Entity<AudioItem>,
        project: Entity<Project>,
        cx: &mut AsyncApp,
    ) -> Result<AudioMetadata> {
        let (fs, audio_path, extension) = cx.update(|cx| {
            let fs = project.read(cx).fs().clone();
            let audio_item = audio.read(cx);
            let audio_path = audio_item
                .abs_path(cx)
                .context("absolutizing audio file path")?;
            let extension = audio_item
                .file
                .path()
                .extension()
                .map(|ext| ext.to_string());
            anyhow::Ok((fs, audio_path, extension))
        })?;

        let audio_bytes = fs.load_bytes(&audio_path).await?;
        Self::compute_metadata_from_bytes(&audio_bytes, extension.as_deref())
    }

    pub fn project_path(&self, cx: &App) -> ProjectPath {
        ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path().clone(),
        }
    }

    pub fn abs_path(&self, cx: &App) -> Option<PathBuf> {
        Some(self.file.as_local()?.abs_path(cx))
    }

    fn file_updated(&mut self, new_file: Arc<worktree::File>, cx: &mut Context<Self>) {
        let mut file_changed = false;

        let old_file = &self.file;
        if new_file.path() != old_file.path() {
            file_changed = true;
        }

        let old_state = old_file.disk_state();
        let new_state = new_file.disk_state();
        if old_state != new_state {
            file_changed = true;
            if matches!(new_state, DiskState::Present { .. }) {
                cx.emit(AudioItemEvent::ReloadNeeded)
            }
        }

        self.file = new_file;
        if file_changed {
            cx.emit(AudioItemEvent::FileHandleChanged);
            cx.notify();
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) -> Option<oneshot::Receiver<()>> {
        let local_file = self.file.as_local()?;
        let (sender, receiver) = futures::channel::oneshot::channel();

        let content = local_file.load_bytes(cx);
        self.reload_task = Some(cx.spawn(async move |this, cx| {
            if let Some(bytes) = content
                .await
                .context("Failed to load audio content")
                .log_err()
            {
                this.update(cx, |this, cx| {
                    this.audio_bytes = Arc::new(bytes);
                    cx.emit(AudioItemEvent::Reloaded);
                })
                .log_err();
            }
            _ = sender.send(());
        }));
        Some(receiver)
    }
}

pub fn is_audio_file(project: &Entity<Project>, path: &ProjectPath, cx: &App) -> bool {
    let extension = util::maybe!({
        let worktree_abs_path = project
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)?
            .read(cx)
            .abs_path();
        path.path
            .extension()
            .or_else(|| worktree_abs_path.extension()?.to_str())
            .map(str::to_lowercase)
    });

    match extension {
        Some(ext) => AUDIO_EXTENSIONS.contains(&ext.as_str()),
        None => false,
    }
}

impl ProjectItem for AudioItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        if is_audio_file(project, path, cx) {
            Some(cx.spawn({
                let path = path.clone();
                let project = project.clone();
                async move |cx| {
                    project
                        .update(cx, |project, cx| project.open_audio(path, cx))
                        .await
                }
            }))
        } else {
            None
        }
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.file.entry_id
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(self.project_path(cx))
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

#[allow(dead_code)]
trait AudioStoreImpl {
    fn open_audio(
        &self,
        path: Arc<RelPath>,
        worktree: Entity<Worktree>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<Entity<AudioItem>>>;

    fn reload_audios(
        &self,
        audios: HashSet<Entity<AudioItem>>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<()>>;

    fn as_local(&self) -> Option<Entity<LocalAudioStore>>;
    fn as_remote(&self) -> Option<Entity<RemoteAudioStore>>;
}

// TODO: Add proto messages for remote audio store support (analogous to CreateImageForPeer, OpenImageByPath, etc.)
struct RemoteAudioStore {
    _upstream_client: AnyProtoClient,
    _project_id: u64,
}

struct LocalAudioStore {
    local_audio_ids_by_path: HashMap<ProjectPath, AudioId>,
    local_audio_ids_by_entry_id: HashMap<ProjectEntryId, AudioId>,
    audio_store: WeakEntity<AudioStore>,
    _subscription: Subscription,
}

pub struct AudioStore {
    state: Box<dyn AudioStoreImpl>,
    opened_audios: HashMap<AudioId, WeakEntity<AudioItem>>,
    worktree_store: Entity<WorktreeStore>,
    #[allow(clippy::type_complexity)]
    loading_audios_by_path: HashMap<
        ProjectPath,
        postage::watch::Receiver<Option<Result<Entity<AudioItem>, Arc<anyhow::Error>>>>,
    >,
}

impl AudioStore {
    pub fn local(worktree_store: Entity<WorktreeStore>, cx: &mut Context<Self>) -> Self {
        let this = cx.weak_entity();
        Self {
            state: Box::new(cx.new(|cx| {
                let subscription = cx.subscribe(
                    &worktree_store,
                    |this: &mut LocalAudioStore, _, event, cx| {
                        if let WorktreeStoreEvent::WorktreeAdded(worktree) = event {
                            this.subscribe_to_worktree(worktree, cx);
                        }
                    },
                );

                LocalAudioStore {
                    local_audio_ids_by_path: Default::default(),
                    local_audio_ids_by_entry_id: Default::default(),
                    audio_store: this,
                    _subscription: subscription,
                }
            })),
            opened_audios: Default::default(),
            loading_audios_by_path: Default::default(),
            worktree_store,
        }
    }

    pub fn remote(
        worktree_store: Entity<WorktreeStore>,
        upstream_client: AnyProtoClient,
        project_id: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            state: Box::new(cx.new(|_| RemoteAudioStore {
                _upstream_client: upstream_client,
                _project_id: project_id,
            })),
            opened_audios: Default::default(),
            loading_audios_by_path: Default::default(),
            worktree_store,
        }
    }

    pub fn audios(&self) -> impl '_ + Iterator<Item = Entity<AudioItem>> {
        self.opened_audios
            .values()
            .filter_map(|audio| audio.upgrade())
    }

    pub fn get(&self, audio_id: AudioId) -> Option<Entity<AudioItem>> {
        self.opened_audios
            .get(&audio_id)
            .and_then(|audio| audio.upgrade())
    }

    pub fn get_by_path(&self, path: &ProjectPath, cx: &App) -> Option<Entity<AudioItem>> {
        self.audios()
            .find(|audio| &audio.read(cx).project_path(cx) == path)
    }

    pub fn open_audio(
        &mut self,
        project_path: ProjectPath,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<AudioItem>>> {
        let existing_audio = self.get_by_path(&project_path, cx);
        if let Some(existing_audio) = existing_audio {
            return Task::ready(Ok(existing_audio));
        }

        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(project_path.worktree_id, cx)
        else {
            return Task::ready(Err(anyhow::anyhow!("no such worktree")));
        };

        let loading_watch = match self.loading_audios_by_path.entry(project_path.clone()) {
            hash_map::Entry::Occupied(entry) => entry.get().clone(),

            hash_map::Entry::Vacant(entry) => {
                let (mut sender, receiver) = postage::watch::channel();
                entry.insert(receiver.clone());

                let load_audio = self
                    .state
                    .open_audio(project_path.path.clone(), worktree, cx);

                cx.spawn(async move |this, cx| {
                    let load_result = load_audio.await;
                    *sender.borrow_mut() = Some(this.update(cx, |this, _cx| {
                        this.loading_audios_by_path.remove(&project_path);
                        let audio = load_result.map_err(Arc::new)?;
                        Ok(audio)
                    })?);
                    anyhow::Ok(())
                })
                .detach();
                receiver
            }
        };

        cx.background_spawn(async move {
            Self::wait_for_loading_audio(loading_watch)
                .await
                .map_err(|error| error.cloned())
        })
    }

    pub async fn wait_for_loading_audio(
        mut receiver: postage::watch::Receiver<
            Option<Result<Entity<AudioItem>, Arc<anyhow::Error>>>,
        >,
    ) -> Result<Entity<AudioItem>, Arc<anyhow::Error>> {
        loop {
            if let Some(result) = receiver.borrow().as_ref() {
                match result {
                    Ok(audio) => return Ok(audio.to_owned()),
                    Err(error) => return Err(error.to_owned()),
                }
            }
            receiver.next().await;
        }
    }

    pub fn reload_audios(
        &self,
        audios: HashSet<Entity<AudioItem>>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<()>> {
        if audios.is_empty() {
            return Task::ready(Ok(()));
        }

        self.state.reload_audios(audios, cx)
    }

    fn add_audio(&mut self, audio: Entity<AudioItem>, cx: &mut Context<AudioStore>) -> Result<()> {
        let audio_id = audio.read(cx).id;
        self.opened_audios.insert(audio_id, audio.downgrade());
        cx.subscribe(&audio, Self::on_audio_event).detach();
        cx.emit(AudioStoreEvent::AudioAdded(audio));
        Ok(())
    }

    fn on_audio_event(
        &mut self,
        audio: Entity<AudioItem>,
        event: &AudioItemEvent,
        cx: &mut Context<Self>,
    ) {
        if let AudioItemEvent::FileHandleChanged = event
            && let Some(local) = self.state.as_local()
        {
            local.update(cx, |local, cx| {
                local.audio_changed_file(audio, cx);
            })
        }
    }
}

impl AudioStoreImpl for Entity<LocalAudioStore> {
    fn open_audio(
        &self,
        path: Arc<RelPath>,
        worktree: Entity<Worktree>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<Entity<AudioItem>>> {
        let this = self.clone();

        let load_file = worktree.update(cx, |worktree, cx| {
            worktree.load_binary_file(path.as_ref(), cx)
        });
        cx.spawn(async move |audio_store, cx| {
            let LoadedBinaryFile { file, content } = load_file.await?;

            let entity = cx.new(|cx| AudioItem {
                id: cx.entity_id().as_non_zero_u64().into(),
                file: file.clone(),
                audio_bytes: Arc::new(content),
                audio_metadata: None,
                reload_task: None,
            });

            let audio_id = cx.read_entity(&entity, |model, _| model.id);

            this.update(cx, |this, cx| {
                audio_store.update(cx, |audio_store, cx| {
                    audio_store.add_audio(entity.clone(), cx)
                })??;
                this.local_audio_ids_by_path.insert(
                    ProjectPath {
                        worktree_id: file.worktree_id(cx),
                        path: file.path.clone(),
                    },
                    audio_id,
                );

                if let Some(entry_id) = file.entry_id {
                    this.local_audio_ids_by_entry_id.insert(entry_id, audio_id);
                }

                anyhow::Ok(())
            })?;

            Ok(entity)
        })
    }

    fn reload_audios(
        &self,
        audios: HashSet<Entity<AudioItem>>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<()>> {
        cx.spawn(async move |_, cx| {
            for audio in audios {
                if let Some(receiver) = audio.update(cx, |audio, cx| audio.reload(cx)) {
                    receiver.await?
                }
            }
            Ok(())
        })
    }

    fn as_local(&self) -> Option<Entity<LocalAudioStore>> {
        Some(self.clone())
    }

    fn as_remote(&self) -> Option<Entity<RemoteAudioStore>> {
        None
    }
}

impl AudioStoreImpl for Entity<RemoteAudioStore> {
    fn open_audio(
        &self,
        _path: Arc<RelPath>,
        _worktree: Entity<Worktree>,
        _cx: &mut Context<AudioStore>,
    ) -> Task<Result<Entity<AudioItem>>> {
        // TODO: Implement remote audio opening once proto messages are added
        Task::ready(Err(anyhow::anyhow!(
            "Opening audio from remote is not yet supported"
        )))
    }

    fn reload_audios(
        &self,
        _audios: HashSet<Entity<AudioItem>>,
        _cx: &mut Context<AudioStore>,
    ) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "Reloading audio from remote is not supported"
        )))
    }

    fn as_local(&self) -> Option<Entity<LocalAudioStore>> {
        None
    }

    fn as_remote(&self) -> Option<Entity<RemoteAudioStore>> {
        Some(self.clone())
    }
}

impl LocalAudioStore {
    fn subscribe_to_worktree(&mut self, worktree: &Entity<Worktree>, cx: &mut Context<Self>) {
        cx.subscribe(worktree, |this, worktree, event, cx| {
            if worktree.read(cx).is_local()
                && let worktree::Event::UpdatedEntries(changes) = event
            {
                this.local_worktree_entries_changed(&worktree, changes, cx);
            }
        })
        .detach();
    }

    fn local_worktree_entries_changed(
        &mut self,
        worktree_handle: &Entity<Worktree>,
        changes: &[(Arc<RelPath>, ProjectEntryId, PathChange)],
        cx: &mut Context<Self>,
    ) {
        let snapshot = worktree_handle.read(cx).snapshot();
        for (path, entry_id, _) in changes {
            self.local_worktree_entry_changed(*entry_id, path, worktree_handle, &snapshot, cx);
        }
    }

    fn local_worktree_entry_changed(
        &mut self,
        entry_id: ProjectEntryId,
        path: &Arc<RelPath>,
        worktree: &Entity<worktree::Worktree>,
        snapshot: &worktree::Snapshot,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let project_path = ProjectPath {
            worktree_id: snapshot.id(),
            path: path.clone(),
        };
        let audio_id = match self.local_audio_ids_by_entry_id.get(&entry_id) {
            Some(&audio_id) => audio_id,
            None => self.local_audio_ids_by_path.get(&project_path).copied()?,
        };

        let audio = self
            .audio_store
            .update(cx, |audio_store, _| {
                if let Some(audio) = audio_store.get(audio_id) {
                    Some(audio)
                } else {
                    audio_store.opened_audios.remove(&audio_id);
                    None
                }
            })
            .ok()
            .flatten();
        let audio = if let Some(audio) = audio {
            audio
        } else {
            self.local_audio_ids_by_path.remove(&project_path);
            self.local_audio_ids_by_entry_id.remove(&entry_id);
            return None;
        };

        audio.update(cx, |audio, cx| {
            let old_file = &audio.file;
            if old_file.worktree != *worktree {
                return;
            }

            let snapshot_entry = old_file
                .entry_id
                .and_then(|entry_id| snapshot.entry_for_id(entry_id))
                .or_else(|| snapshot.entry_for_path(old_file.path.as_ref()));

            let new_file = if let Some(entry) = snapshot_entry {
                worktree::File {
                    disk_state: match entry.mtime {
                        Some(mtime) => DiskState::Present { mtime },
                        None => old_file.disk_state,
                    },
                    is_local: true,
                    entry_id: Some(entry.id),
                    path: entry.path.clone(),
                    worktree: worktree.clone(),
                    is_private: entry.is_private,
                }
            } else {
                worktree::File {
                    disk_state: DiskState::Deleted,
                    is_local: true,
                    entry_id: old_file.entry_id,
                    path: old_file.path.clone(),
                    worktree: worktree.clone(),
                    is_private: old_file.is_private,
                }
            };

            if new_file == **old_file {
                return;
            }

            if new_file.path != old_file.path {
                self.local_audio_ids_by_path.remove(&ProjectPath {
                    path: old_file.path.clone(),
                    worktree_id: old_file.worktree_id(cx),
                });
                self.local_audio_ids_by_path.insert(
                    ProjectPath {
                        worktree_id: new_file.worktree_id(cx),
                        path: new_file.path.clone(),
                    },
                    audio_id,
                );
            }

            if new_file.entry_id != old_file.entry_id {
                if let Some(entry_id) = old_file.entry_id {
                    self.local_audio_ids_by_entry_id.remove(&entry_id);
                }
                if let Some(entry_id) = new_file.entry_id {
                    self.local_audio_ids_by_entry_id.insert(entry_id, audio_id);
                }
            }

            audio.file_updated(Arc::new(new_file), cx);
        });
        None
    }

    fn audio_changed_file(&mut self, audio: Entity<AudioItem>, cx: &mut App) -> Option<()> {
        let audio = audio.read(cx);
        let file = &audio.file;

        let audio_id = audio.id;
        if let Some(entry_id) = file.entry_id {
            match self.local_audio_ids_by_entry_id.get(&entry_id) {
                Some(_) => {
                    return None;
                }
                None => {
                    self.local_audio_ids_by_entry_id.insert(entry_id, audio_id);
                }
            }
        };
        self.local_audio_ids_by_path.insert(
            ProjectPath {
                worktree_id: file.worktree_id(cx),
                path: file.path.clone(),
            },
            audio_id,
        );

        Some(())
    }
}
