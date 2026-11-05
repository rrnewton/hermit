//! `vibe-talk` — a chat bridge for a voice agent and phone web app.
//!
//! The server owns the front door. A hosted voice agent (ElevenLabs, in the first intended
//! deployment) reaches this process over HTTP, asks it what has been said in a chat channel,
//! and — with explicit approval — asks it to post a reply.
//!
//! # One thing here pushes
//!
//! This used to say "nothing here pushes: every read is pulled at the moment the owner asks a
//! question", and that stopped being true with [`live`]. A deployment chooses one producer:
//! bounded built-in polling with `discord.live_poll_seconds`, or authenticated provider-neutral
//! adapter push with `ingest.token`. Both fan changes out through
//! `GET /api/v1/channels/{id}/stream`, so a page learns about them without asking. Both are OFF
//! unless configured; the channel allowlist applies to ingestion and to the browser stream.
//!
//! # What is kept
//!
//! Almost nothing. Nothing about a channel is written to disk, and the page and the tools read
//! channels from the provider, with two short-lived exceptions, held in memory only, kept apart on
//! purpose, and gone when the process restarts. [`read_ahead`] keeps messages fetched in the last
//! thirty seconds, and only speech preparation reads them, so a tap does not wait to re-fetch what
//! the page has just drawn; a pushed edit or deletion drops a message from it, and the storage
//! purge empties it. [`recent`] keeps each channel's newest window, with every change pushed to it
//! since merged in, so the voice agent's catch-up can answer from it while the provider is slow,
//! and says how old it is when it was not read again in time; the storage purge empties that too,
//! with the pushes kept beside it. Neither can be refilled by a read that was already running when
//! the purge came. They are not the only channel text in memory, though, and the purge does not
//! empty the rest: the live stream's replay tail, read-aloud tickets and the thread names already
//! read among them, as `http::api::purge_storage` lists. [`store`] is the single thing kept
//! at rest, and it holds three things — the `/voice` transcript,
//! how far the owner has read, and one short cached SUMMARY per long message. The first two are
//! what this server itself authored; the third is derived from somebody else's message — a
//! model's paraphrase of one, which carries its content as surely as a quotation would — so it is
//! treated as a second at-rest copy of third-party text: bounded by the same retention, erased by
//! the same purge, and never written on behalf of a read-scope token. Read state in particular is OURS:
//! Discord shares none with a bot, so nothing here is synchronised with it in either direction.
//! [`cache`] is a disposable on-disk copy of channel messages in a file of its own
//! (`#208 persistent-cache`); in this build nothing opens it, so it keeps nothing.
//!
//! # Untrusted input
//!
//! Discord message content is written by other parties. Everything this crate returns from a
//! channel is DATA, never instructions, and it is the caller's job to keep treating it that way.
//! [`untrusted`] exists to make that boundary explicit at the one place where channel text is
//! handed to a language model.

#![forbid(unsafe_code)]

pub mod access;
pub mod agent_backend;
pub mod auth;
pub mod cache;
pub mod chat;
pub mod clock;
pub mod config;
pub mod contract;
pub mod conversation;
pub mod diagnostics;
pub mod directory;
pub mod discord;
pub mod elevenlabs;
pub mod heading;
pub mod http;
pub mod link_labels;
pub mod live;
pub mod mcp;
pub mod model;
pub mod noise;
pub mod ops;
pub mod post_gate;
pub mod post_lanes;
pub mod probe;
pub mod providers;
pub mod read_ahead;
pub mod recent;
pub mod recent_view;
pub mod render;
pub mod replay;
pub mod retrieval;
pub mod send_approval;
pub mod slack;
pub mod speakable;
pub mod speech;
pub mod speech_tickets;
pub mod state;
pub mod store;
pub mod summarize;
pub mod summary;
pub mod testing;
pub mod threads;
pub mod timeline_forward;
pub mod untrusted;
pub mod upstream_clear;
pub mod upstream_profile;
pub mod voice_agent;
pub mod voice_health;
pub mod voice_prompt;
