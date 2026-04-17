# Sample Nomad server HCL configuration
# This is a fixture for testing configuration generation and comparison

datacenter = "dc1"
region     = "global"

server {
  enabled          = true
  bootstrap_expect = 1
  encrypt          = "uuua8jhasdfjhasdfh=="
}

# High-latency tuning example
telemetry {
  prometheus_metrics = true
}

acl {
  enabled = false
}
