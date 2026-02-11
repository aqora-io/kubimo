group "default" {
  targets = ["marimo", "controller"]
}

target "docker-metadata-controller" {}

target "controller" {
  inherits = ["docker-metadata-controller"]
  dockerfile = "docker/Dockerfile.controller"
  context = "."
  platforms = [BAKE_LOCAL_PLATFORM]
}

target "docker-metadata-marimo" {}

target "marimo" {
  inherits = ["docker-metadata-marimo"]
  dockerfile = "docker/Dockerfile.marimo"
  context = "."
  platforms = [BAKE_LOCAL_PLATFORM]
}
