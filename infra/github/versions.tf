terraform {
  required_version = ">= 1.6"
  required_providers {
    github = {
      source  = "integrations/github"
      version = "~> 6.13"
    }
  }
}

# Auth comes from GITHUB_TOKEN in the environment of the terraform command the
# owner runs by hand. Nothing in this directory holds a credential.
provider "github" {
  owner = var.owner
}
