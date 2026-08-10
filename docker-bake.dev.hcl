variable "TAG" {
  default = "dev"
}

target "docker-metadata-controller" {
  tags = ["ghcr.io/aqora-io/kubimo-controller:${TAG}"]
}

target "docker-metadata-marimo" {
  tags = ["ghcr.io/aqora-io/kubimo-marimo:${TAG}"]
}

target "docker-metadata-conda-marimo" {
  tags = ["ghcr.io/aqora-io/kubimo-conda-marimo:${TAG}"]
}

target "docker-metadata-agent" {
  tags = ["ghcr.io/aqora-io/kubimo-agent:${TAG}"]
}
