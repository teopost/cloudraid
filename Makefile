# Compila e testa dentro un container Rust: sul server non serve installare niente.
IMAGE  ?= rust:1-bookworm
# i test dei backend rclone usano il rclone dell'host, se c'è (altrimenti vengono saltati)
RCLONE := $(shell command -v rclone 2>/dev/null)
DOCKER  = docker run --rm -v $(CURDIR):/src -w /src -e CARGO_HOME=/src/.cargo -e HOME=/tmp \
          $(if $(RCLONE),-v $(RCLONE):/usr/local/bin/rclone:ro) \
          -u $$(id -u):$$(id -g) $(IMAGE)

.DEFAULT_GOAL := help
.PHONY: help build test clean

help: ## Mostra questo aiuto
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-8s\033[0m %s\n", $$1, $$2}'

build: ## Compila il binario in target/release/cloudraid
	$(DOCKER) cargo build --release

test: ## Lancia tutti i test
	$(DOCKER) cargo test --release

clean: ## Cancella i file di compilazione
	rm -rf target
