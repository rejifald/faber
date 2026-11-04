variable "owner" {
  description = "Account that owns the repository."
  type        = string
  default     = "rejifald"
}

variable "repository" {
  description = "Repository name. Point this at a throwaway repository to rehearse."
  type        = string
  default     = "faber"
}

variable "ci_check_contexts" {
  description = "Status check names main requires. These are the job names in .github/workflows/ci.yml and privacy-text.yml, and must match them exactly. The macOS jobs are not among them: they skip on pull requests and run only on a CI dispatch on main, which release.yml requires."
  type        = list(string)
  default = [
    "linux · fmt · clippy · test · ui · infra",
    "privacy · text",
  ]
}

variable "owner_bypass_main_for_cut" {
  description = <<-EOT
    Let the repository admin push to main directly. ADR-0031 allows this for
    the cut's single first commit only. Leave false; set true for that one
    apply, then apply again with it back at false.
  EOT
  type        = bool
  default     = false
}

variable "secret_scanning" {
  description = "Turn on secret scanning and push protection. GitHub allows them on a public repository only, so the apply that makes the repository public sets this false, and the next apply, with the default, turns them on."
  type        = bool
  default     = true
}

variable "privacy_key" {
  description = <<-EOT
    The privacy gate's key (ADR-0058), as hex: the Actions secret
    FABER_PRIVACY_KEY. Take it from the owner's keychain:
    export TF_VAR_privacy_key="$(security find-generic-password -s faber-privacy-key -w)".
    It is kept in the local state, which never leaves the owner's machine.
  EOT
  type        = string
  sensitive   = true

  validation {
    condition     = can(regex("^([0-9a-fA-F]{2}){32,}$", var.privacy_key))
    error_message = "privacy_key must be at least 32 bytes of hex (64 digits)."
  }
}
