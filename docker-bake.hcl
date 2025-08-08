variable "VERSION" {
  default = "test"
}

variable "REGISTRY" {
  default = "local"
}

variable "MARIMO_BASE_NAME" {
  default = "kubimo-marimo-base"
}

variable "MARIMO_INIT_NAME" {
  default = "kubimo-marimo-init"
}

group "default" {
  targets = ["marimo-base", "marimo-init"]
}

target "marimo" {
  dockerfile = "Dockerfile.marimo"
  context = "."
}

target "marimo-base" {
  name = "marimo-base-${version}"
  inherits = ["marimo"]
  target = "base"
  matrix = {
    version = split(",", VERSION)
  }
  tags = [ "${REGISTRY}/${MARIMO_BASE_NAME}:${version}" ]
}

target "marimo-init" {
  name = "marimo-init-${version}"
  inherits = ["marimo"]
  target = "init"
  matrix = {
    version = split(",", VERSION)
  }
  tags = [ "${REGISTRY}/${MARIMO_INIT_NAME}:${version}" ]
}
