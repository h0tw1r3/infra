# Sample Nomad client HCL configuration
# This is a fixture for testing configuration generation and comparison

datacenter = "dc1"
region     = "global"

client {
  enabled   = true
  node_pool = "default"
}

servers = [
  "10.0.1.1:4647"
]
