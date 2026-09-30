# M-Spike acceptance fixture: `lodi develop -- make` builds and runs a libssl-linked program.
# Every build product is written below .lodi/ (OUT_DIR, set in lodi.toml [env]).
OUT_DIR ?= .lodi/build
PROGRAM := $(OUT_DIR)/openssl-version
CFLAGS  += -O2 -Wall -Werror $(shell pkg-config --cflags openssl)
LDLIBS  += $(shell pkg-config --libs openssl)

.PHONY: all run
all: run

$(PROGRAM): openssl-version.c
	mkdir -p $(OUT_DIR)
	$(CC) $(CFLAGS) -o $@ $< $(LDLIBS)

run: $(PROGRAM)
	$(PROGRAM)
	ldd $(PROGRAM) | grep -E 'lib(ssl|crypto)\.so'
