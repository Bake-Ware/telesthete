# Build the C producer example.
#
# Run from the rust/ workspace root:
#   cargo build --release -p telesthete-c
#   make -C examples/c-producer
#   LD_LIBRARY_PATH=../../target/release ./tlt-c-producer
#
# Or invoke from this dir with the same env.

WORKSPACE_ROOT := $(realpath ../..)
LIBDIR         := $(WORKSPACE_ROOT)/target/release
INCDIR         := $(WORKSPACE_ROOT)/telesthete/include

CFLAGS  ?= -O2 -Wall -Wextra -std=c11
LDFLAGS ?= -L$(LIBDIR) -ltelesthete -Wl,-rpath,$(LIBDIR)

tlt-c-producer: main.c $(INCDIR)/telesthete.h
	$(CC) $(CFLAGS) -I$(INCDIR) main.c -o $@ $(LDFLAGS)

clean:
	rm -f tlt-c-producer

.PHONY: clean
