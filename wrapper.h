/*
 * wrapper.h --- bindgen entry point.
 *
 * Pulls in contrib/otel's public api header along with the minimal
 * postgres machinery it transitively depends on (TimestampTz from
 * datatype/timestamp.h, etc.).  We include the full postgres.h
 * umbrella because that's the only way to satisfy otel.h's
 * transitive #includes without forking copies of postgres internals
 * into this crate.  bindgen runs once at build time so the bloat
 * is paid once; the actual Rust code emitted by bindgen is filtered
 * to a small allow-list of symbols via build.rs.
 */
#include "postgres.h"
#include "fmgr.h"
#include <otel/otel.h>
