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
  # feat-ssr branch of our fork
  default = "https://github.com/aqora-io/marimo.git#77500f5fd71c093efc60770e7fd1ce010d99ab0d"
}

group "default" {
  targets = ["marimo", "conda-marimo", "controller", "agent"]
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
target "docker-metadata-conda-marimo" {}

target "marimo" {
  inherits = ["docker-metadata-marimo"]
  target = "uv"
  dockerfile = "docker/Dockerfile.marimo"
  context = "."
  # platforms = [BAKE_LOCAL_PLATFORM]
  args = {
    MARIMO_GIT = MARIMO_GIT
  }
}

target "conda-marimo" {
  inherits = ["marimo", "docker-metadata-conda-marimo"]
  target = "pixi"
}
