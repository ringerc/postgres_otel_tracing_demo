/*
 * Postgres module magic.
 *
 * Every postgres loadable module needs a function named Pg_magic_func
 * whose returned struct postgres consults at dlopen time to verify ABI
 * compatibility.  PG_MODULE_MAGIC is the canonical macro for that, but
 * we can't use it here directly: the function name it produces would
 * collide with the #[no_mangle] Rust wrapper that re-exports the
 * symbol into the cdylib's dynamic symbol table (Rust's cdylib
 * link applies a version script that hides any symbol not declared
 * via #[no_mangle]).
 *
 * Instead, manually expand the macro into a uniquely-named function;
 * Rust calls it from the wrapper named Pg_magic_func.  The contents
 * of the returned struct are identical to what PG_MODULE_MAGIC would
 * have produced.
 */
#include "postgres.h"
#include "fmgr.h"

PGDLLEXPORT const Pg_magic_struct *pg_magic_data_get(void);

const Pg_magic_struct *
pg_magic_data_get(void)
{
	static const Pg_magic_struct Pg_magic_data =
		PG_MODULE_MAGIC_DATA(.name = NULL);
	return &Pg_magic_data;
}
