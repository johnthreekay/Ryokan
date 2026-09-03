pub mod airing_refresh;
pub mod anilist;
pub mod calendar;
pub mod crypto;
pub mod download_client;
pub mod external_sync;
pub mod indexer_catalog;
pub mod indexers;
pub mod jikan;
pub mod kitsu;
pub mod mal;
pub mod media;
pub mod nyaa;
pub mod oauth_state;
pub mod sanitize;
pub mod scoring;

pub mod jellyfin;

pub mod auto_expand;
pub mod auto_search;
pub mod backup;
pub mod grab_commit;
pub mod grab_sweep;
pub mod interactive_search_cache;
pub mod library_link;
pub mod logger;
pub mod manual_import;
pub mod progress;
pub mod quality;
pub mod relative_time;
pub mod task_registry;

pub mod rss;

pub mod monitoring;

pub mod metadata_sync;
pub mod misgrab;

pub mod artwork;

pub mod html;

pub mod naming;
pub mod nfo;
pub mod notifications;
pub mod post_processing;
pub mod recycle;

pub mod anibridge;
pub mod upgrade;

pub mod custom_formats;
pub mod seadex;

// Classification pipeline (Phase 1a foundations).
pub mod source;
pub mod source_description;
pub mod source_dir;
pub mod source_ffprobe;
pub mod source_filename;
pub mod source_groups;
pub mod source_temporal;

pub mod user_score;
