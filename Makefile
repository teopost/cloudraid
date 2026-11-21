# Compila e testa dentro un container Rust: sul server non serve installare niente.
# La cache di compilazione sta fuori da /srv/docker, così non finisce nei backup.
IMAGE  ?= rust:1-bookworm
CACHE  ?= $(HOME)/.cache/cloudraid-build
# i test dei backend rclone usano il rclone dell'host, se c'è (altrimenti vengono saltati)
RCLONE := $(shell command -v rclone 2>/dev/null)
DOCKER  = docker run --rm -v $(CURDIR):/src -v $(CACHE):/cache -w /src \
          -e CARGO_HOME=/cache/cargo -e CARGO_TARGET_DIR=/cache/target -e HOME=/tmp \
          $(if $(RCLONE),-v $(RCLONE):/usr/local/bin/rclone:ro) \
          -u $$(id -u):$$(id -g) $(IMAGE)

.DEFAULT_GOAL := help
.PHONY: help build test clean

help: ## Mostra questo aiuto
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-8s\033[0m %s\n", $$1, $$2}'

build: ## Costruisce l'immagine Docker (cloudraid + rclone)
	docker compose build web

test: ## Lancia tutti i test
	@mkdir -p $(CACHE)
	$(DOCKER) cargo test --release

clean: ## Cancella la cache di compilazione
	rm -rf $(CACHE)
