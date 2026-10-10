//! Telegram over TDLib's JSON interface. TDLib speaks MTProto, keeps the local database and
//! repairs update gaps; this module only loads it, routes its JSON, and types the subset of
//! the schema Avro reads.
pub mod auth;
pub mod client;
pub mod ffi;
pub mod types;
