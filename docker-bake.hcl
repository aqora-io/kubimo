variable "VERSION" {
  default = "dev"
}

variable "REGISTRY" {
  default = "local"
}

variable "MARIMO_NAME" {
  default = "kubimo-marimo"
}

group "default" {
  targets = ["marimo"]
}

target "marimo" {
  dockerfile = "docker/Dockerfile.marimo"
  context = "."
  name = "marimo-${version}"
  matrix = {
    version = split(",", VERSION)
  }
  tags = [ trimprefix("${REGISTRY}/${MARIMO_NAME}:${version}", "/") ]
}
