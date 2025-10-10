group "default" {
  targets = ["marimo", "controller"]
}

target "docker-metadata-controller" {}

target "controller" {
  inherits = ["docker-metadata-controller"]
  dockerfile = "docker/Dockerfile.controller"
  context = "."
}

target "docker-metadata-marimo" {}

target "marimo" {
  inherits = ["docker-metadata-marimo"]
  dockerfile = "docker/Dockerfile.marimo"
  context = "."
}
