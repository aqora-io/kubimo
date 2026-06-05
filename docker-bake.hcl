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
  default = "https://github.com/aqora-io/marimo.git#2a29d2f51819558c7da9fbf7cc81671f723cf412"
}

group "default" {
  targets = ["marimo", "controller"]
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

target "docker-metadata-marimo" {}

target "marimo" {
  inherits = ["docker-metadata-marimo"]
  dockerfile = "docker/Dockerfile.marimo"
  context = "."
  # platforms = [BAKE_LOCAL_PLATFORM]
  args = {
    SCCACHE_ENDPOINT = SCCACHE_ENDPOINT
    SCCACHE_BUCKET   = SCCACHE_BUCKET
    SCCACHE_REGION   = SCCACHE_REGION
    MARIMO_GIT       = MARIMO_GIT
  }
  secret = [
    "id=SCCACHE_AWS_ACCESS_KEY_ID,env=SCCACHE_AWS_ACCESS_KEY_ID",
    "id=SCCACHE_AWS_SECRET_ACCESS_KEY,env=SCCACHE_AWS_SECRET_ACCESS_KEY",
  ]
}
