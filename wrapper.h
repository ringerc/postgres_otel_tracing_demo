/*
 * wrapper.h --- bindgen entry point.
 *
 * Pulls in otel_api's exporter-facing header (otel_exporter.h, which
 * in turn pulls in otel_api.h and otel_types.h) along with the minimal
 * postgres machinery it transitively depends on (TimestampTz from
 * datatype/timestamp.h, etc.).  We include the full postgres.h
 * umbrella because that's the only way to satisfy otel_exporter.h's
 * transitive #includes without forking copies of postgres internals
 * into this crate.  bindgen runs once at build time so the bloat
 * is paid once; the actual Rust code emitted by bindgen is filtered
 * to a small allow-list of symbols via build.rs.
 *
 * We deliberately include otel_exporter.h rather than the otel.h
 * umbrella: this crate is an exporter and has no business with the
 * producer or internal-api surface (otel_producer.h,
 * otel_internal_api.h).
 */
#include "postgres.h"
#include "fmgr.h"
#include "utils/guc.h"			/* MarkGUCPrefixReserved */
#include <otel_api/otel_exporter.h>
