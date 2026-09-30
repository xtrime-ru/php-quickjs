# php-quickjs — build & test
#
# The extension is a plain cargo cdylib (no phpize). Load the built .so by
# absolute path with `php -d extension=...`.

PROFILE ?= debug
ifeq ($(PROFILE),release)
CARGO_FLAGS := --release
else
CARGO_FLAGS :=
endif

EXT_SUFFIX := $(if $(filter Darwin,$(shell uname -s)),dylib,so)
TARGET_DIR := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),$(CURDIR)/target)
EXT := $(TARGET_DIR)/$(PROFILE)/libphp_quickjs.$(EXT_SUFFIX)
PHP := php -d extension=$(EXT)

.PHONY: all build release test test-rust test-php test-docker stubs example clean fmt

all: build

build:
	cargo build $(CARGO_FLAGS)

release:
	$(MAKE) build PROFILE=release

# Rust unit tests (marshaling, manifest, facade) + the PHP integration suite.
test: build test-rust test-php

test-docker:
	docker compose run --rm --build dev

test-rust:
	cargo test --lib

test-php: build
	@fail=0; \
	for t in tests/php/[0-9]*.php; do \
	  printf '\n=== %s ===\n' "$$t"; \
	  $(PHP) "$$t" || fail=1; \
	done; \
	exit $$fail

# Regenerate the IDE stub for the PHP-facing classes (requires cargo-php:
#   cargo install cargo-php).
stubs:
	@tmp=$$(mktemp stubs/php_quickjs.stubs.php.XXXXXX); \
	trap 'rm -f "$$tmp"' EXIT HUP INT TERM; \
	cargo php stubs --stdout > "$$tmp" && mv "$$tmp" stubs/php_quickjs.stubs.php

example: build
	@for ex in examples/*.php; do \
	  printf '\n=== %s ===\n' "$$ex"; \
	  $(PHP) "$$ex"; \
	done

fmt:
	cargo fmt

clean:
	cargo clean
