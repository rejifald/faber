# ADR-0031's repository rules as code. See README.md for how the owner applies
# them and for the settings Terraform cannot express.

resource "github_repository" "this" {
  name        = var.repository
  description = "Your own CI runners, on your own hardware. A fresh, isolated GitHub Actions runner for every job, on your Mac."
  visibility  = "public"

  has_issues   = true
  has_projects = false
  has_wiki     = false

  allow_merge_commit     = true
  allow_rebase_merge     = false
  allow_squash_merge     = true
  delete_branch_on_merge = true

  # Never let `terraform destroy` delete the repository.
  archive_on_destroy = true

  # Secret scanning and push protection. GitHub allows them on a public
  # repository only, so the apply that makes this one public runs with
  # `-var secret_scanning=false`, and the next apply turns them on (README).
  dynamic "security_and_analysis" {
    for_each = var.secret_scanning ? [1] : []
    content {
      secret_scanning {
        status = "enabled"
      }
      secret_scanning_push_protection {
        status = "enabled"
      }
    }
  }

  # NOT expressible in provider 6.13: immutable releases. Set by hand, see README.
}

# Dependabot alerts, and the pull requests that fix them.
resource "github_repository_vulnerability_alerts" "this" {
  repository = github_repository.this.name
  enabled    = true
}

resource "github_repository_dependabot_security_updates" "this" {
  repository = github_repository.this.name
  enabled    = true

  depends_on = [github_repository_vulnerability_alerts.this]
}

# The default workflow token is read-only; a workflow widens it per job.
resource "github_workflow_repository_permissions" "this" {
  repository                       = github_repository.this.name
  default_workflow_permissions     = "read"
  can_approve_pull_request_reviews = false
}

# GitHub-owned actions only, plus the third-party actions ci.yml uses today,
# allow-listed by pattern until replaced, and every action pinned to a full
# commit SHA. tests/workflow_pins.rs checks the same before anything is pushed.
resource "github_actions_repository_permissions" "this" {
  repository           = github_repository.this.name
  allowed_actions      = "selected"
  sha_pinning_required = true
  allowed_actions_config {
    github_owned_allowed = true
    verified_allowed     = false
    patterns_allowed = [
      "dtolnay/rust-toolchain@*",
      "Swatinem/rust-cache@*",
      "hashicorp/setup-terraform@*",
    ]
  }
}

# The privacy gate's key (ADR-0058). The tree publishes the gate's digests, and
# only this key stops them being guessed, so it lives here and in the owner's
# keychain, never in the tree.
resource "github_actions_secret" "privacy_key" {
  repository  = github_repository.this.name
  secret_name = "FABER_PRIVACY_KEY"
  value       = var.privacy_key
}

# v* tags: created only by Publish (the owner), never moved or deleted. Only the
# repository admin may bypass.
resource "github_repository_ruleset" "release_tags" {
  name        = "release-tags"
  repository  = github_repository.this.name
  target      = "tag"
  enforcement = "active"

  conditions {
    ref_name {
      include = ["refs/tags/v*"]
      exclude = []
    }
  }

  rules {
    creation         = true
    update           = true
    deletion         = true
    non_fast_forward = true
  }

  # Repository admin (role id 5) = the owner on a user-owned repository.
  bypass_actors {
    actor_type  = "RepositoryRole"
    actor_id    = 5
    bypass_mode = "always"
  }
}

# main: pull request required, CI green, no force-push, no deletion. No bypass,
# except the cut's one first commit when explicitly enabled.
resource "github_repository_ruleset" "main" {
  name        = "main"
  repository  = github_repository.this.name
  target      = "branch"
  enforcement = "active"

  conditions {
    ref_name {
      include = ["~DEFAULT_BRANCH"]
      exclude = []
    }
  }

  rules {
    deletion         = true
    non_fast_forward = true

    pull_request {
      required_approving_review_count = 0
    }

    required_status_checks {
      strict_required_status_checks_policy = true
      dynamic "required_check" {
        for_each = var.ci_check_contexts
        content {
          context = required_check.value
        }
      }
    }
  }

  dynamic "bypass_actors" {
    for_each = var.owner_bypass_main_for_cut ? [1] : []
    content {
      actor_type  = "RepositoryRole"
      actor_id    = 5
      bypass_mode = "always"
    }
  }
}
