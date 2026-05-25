# Makefile shim around cargo, mirroring the standard postgres
# loadable-module install layout so this crate can be built and
# installed using the same `make && sudo make install` muscle memory
# as a contrib module.
#
# pg_config tells us where the produced .so should land and where
# contrib/otel's installed header lives (used by the bindgen step
# in build.rs).

PG_CONFIG ?= pg_config
PKGLIBDIR := $(shell $(PG_CONFIG) --pkglibdir)

CARGO ?= cargo
CARGO_PROFILE ?= release
CARGO_FLAGS := --release

# The cdylib name is libpostgres_otel_tracing_demo.so on Linux,
# .dylib on macOS, .dll on Windows.  We install it as
# postgres_otel_tracing_demo.so under pkglibdir which is the name
# postgres dlsyms against (no lib- prefix).
ifeq ($(shell uname -s),Darwin)
  CRATE_LIB := target/$(CARGO_PROFILE)/libpostgres_otel_tracing_demo.dylib
  INSTALLED_LIB := $(PKGLIBDIR)/postgres_otel_tracing_demo.dylib
else
  CRATE_LIB := target/$(CARGO_PROFILE)/libpostgres_otel_tracing_demo.so
  INSTALLED_LIB := $(PKGLIBDIR)/postgres_otel_tracing_demo.so
endif

.PHONY: all build install uninstall clean check

all: build

build:
	PG_CONFIG=$(PG_CONFIG) $(CARGO) build $(CARGO_FLAGS)

# Install needs root for the typical /usr/local/pgsql/lib path; the
# user is expected to run `sudo make install`.
install: build
	install -d $(PKGLIBDIR)
	install -m 755 $(CRATE_LIB) $(INSTALLED_LIB)
	@echo
	@echo "Installed: $(INSTALLED_LIB)"
	@echo "Add 'otel,postgres_otel_tracing_demo' to shared_preload_libraries"
	@echo "(otel MUST come first; postgres_otel_tracing_demo locates it via"
	@echo " the OtelTracingApi rendezvous variable at _PG_init time)."

uninstall:
	rm -f $(INSTALLED_LIB)

check:
	PG_CONFIG=$(PG_CONFIG) $(CARGO) check $(CARGO_FLAGS)

clean:
	$(CARGO) clean
