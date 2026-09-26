variable "SCCACHE_ENDPOINT" {
  default = ""
}

variable "SCCACHE_BUCKET" {
  default = ""
}

variable "SCCACHE_REGION" {
  default = "auto"
}

variable "MARIMO_GIT" {
  # aqora-io branch of our fork
  default = "https://github.com/aqora-io/marimo.git#f3f8f996034e50049be0b510da33bf4d03a05e99"
}

group "default" {
  targets = ["marimo", "controller", "agent"]
}

target "docker-metadata-controller" {}

target "controller" {
  inherits = ["docker-metadata-controller"]
  dockerfile = "docker/Dockerfile.controller"
  context = "."
  # platforms = [BAKE_LOCAL_PLATFORM]
  args = {
    SCCACHE_ENDPOINT = SCCACHE_ENDPOINT
    SCCACHE_BUCKET   = SCCACHE_BUCKET
    SCCACHE_REGION   = SCCACHE_REGION
  }
  secret = [
    "id=SCCACHE_AWS_ACCESS_KEY_ID,env=SCCACHE_AWS_ACCESS_KEY_ID",
    "id=SCCACHE_AWS_SECRET_ACCESS_KEY,env=SCCACHE_AWS_SECRET_ACCESS_KEY",
  ]
}

target "docker-metadata-agent" {}

target "agent" {
  inherits   = ["docker-metadata-agent"]
  dockerfile = "docker/Dockerfile.agent"
  context    = "."
  args = {
    SCCACHE_ENDPOINT = SCCACHE_ENDPOINT
    SCCACHE_BUCKET   = SCCACHE_BUCKET
    SCCACHE_REGION   = SCCACHE_REGION
  }
  secret = [
    "id=SCCACHE_AWS_ACCESS_KEY_ID,env=SCCACHE_AWS_ACCESS_KEY_ID",
    "id=SCCACHE_AWS_SECRET_ACCESS_KEY,env=SCCACHE_AWS_SECRET_ACCESS_KEY",
  ]
}

target "docker-metadata-marimo" {}

target "marimo" {
  inherits = ["docker-metadata-marimo"]
  target = "marimo"
  dockerfile = "docker/Dockerfile.marimo"
  context = "."
  # platforms = [BAKE_LOCAL_PLATFORM]
  args = {
    MARIMO_GIT = MARIMO_GIT
  }
}

# Runs the image's test stage: never pushed, so it is not in the default
# group and its output is discarded once the build (and its RUN pytest) has
# either passed or failed.
target "marimo-test" {
  inherits = ["marimo"]
  target   = "test"
  output   = ["type=cacheonly"]
}
