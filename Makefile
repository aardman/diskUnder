BIN      := diskunder
RELEASE  := target/release/$(BIN)
LINUX    := target/x86_64-unknown-linux-musl/release/$(BIN)

.PHONY: all mac linux clean

all: mac linux

mac:
	cargo build --release
	cp $(RELEASE) $(BIN)-macos
	@echo "Built: $(BIN)-macos"

linux:
	cargo build --release --target x86_64-unknown-linux-musl
	cp $(LINUX) $(BIN)-linux
	@echo "Built: $(BIN)-linux"

clean:
	cargo clean
	rm -f $(BIN)-macos $(BIN)-linux
