variable "VERSION" {
  default = "latest"
}

variable "REGISTRY" {
  default = "ghcr.io/aqora-io"
}

variable "MARIMO_NAME" {
  default = "kubimo-marimo"
}

variable "CONTROLLER_NAME" {
  default = "kubimo-controller"
}

group "default" {
  targets = ["marimo", "controller"]
}

target "controller" {
  dockerfile = "docker/Dockerfile.controller"
  context = "."
  name = "controller-${replace(version, ".", "-")}"
  matrix = {
    version = split(",", VERSION)
  }
  tags = [ trimprefix("${REGISTRY}/${CONTROLLER_NAME}:${version}", "/") ]
}

target "marimo" {
  dockerfile = "docker/Dockerfile.marimo"
  context = "."
  name = "marimo-${replace(version, ".", "-")}"
  matrix = {
    version = split(",", VERSION)
  }
  tags = [ trimprefix("${REGISTRY}/${MARIMO_NAME}:${version}", "/") ]
}
