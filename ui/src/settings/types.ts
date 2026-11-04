// The Settings payloads as the daemon sends them (src/settings.rs FieldSpec and SectionSpec,
// src/web.rs SettingsView and SaveResult). Optional keys are absent from the JSON when unset.

/** A config object: the file's TOML as JSON, read by dotted key. */
export type Config = Record<string, unknown>;

export type FieldKind = 'text' | 'int' | 'float' | 'bool' | 'enum' | 'ceiling' | 'csv' | 'lines' | 'map';

/** Which edit of a `sudo` list or map narrows it, so the form may make that edit (anatomy.md § 2). */
export type Narrows = 'remove' | 'remove-keep-one' | 'add';

/** How a list that takes additions in the form checks one. The daemon validates the same pattern. */
export interface ItemSpec {
  /** A regular expression source the whole item must match. */
  pattern: string;
  /** An example item, shown as the placeholder. */
  example: string;
  /** The message when the item does not match. */
  error: string;
}

/** The one narrowing a read-only field offers: its label and the value it saves, as a JSON literal. */
export interface Narrow {
  label: string;
  value: string;
}

export interface FieldSpec {
  /** Dotted path into the config object (`autolimit.margin`). */
  key: string;
  label: string;
  kind: FieldKind;
  hint?: string;
  unit?: string;
  /** Choices of an enum, or the three labels `[unset, true, false]` of an optional bool. */
  options?: string[];
  min?: number;
  step?: number;
  /** A save pushes it straight into the running daemon; otherwise it waits for a restart. */
  live: boolean;
  /** An empty input means unset (`null`). */
  optional: boolean;
  /** What a blank optional int or bool saves as, where unset is a state of its own. */
  unstated_saves?: number;
  /** The command that changes a read-only field; `<profile>` stands for the profile's name. */
  sudo?: string;
  narrow?: Narrow;
  /** For a `sudo` list or map: the edit the form may make. Absent: read-only. */
  narrows?: Narrows;
  /** For a list that takes additions in the form. */
  item?: ItemSpec;
}

export interface SectionSpec {
  title: string;
  hint?: string;
  fields: FieldSpec[];
}

export interface FormSchema {
  sections: SectionSpec[];
  profile_fields: FieldSpec[];
}

export interface LiveOverride {
  key: string;
  file: unknown;
  live: unknown;
}

export interface SettingsView {
  path: string;
  exists: boolean;
  error?: string;
  from_flags: boolean;
  /** The digest of the file as read; send it back as `base` so a save of a changed file answers 409. */
  base?: string;
  port: number;
  config: Config;
  defaults: Config;
  pending_restart: string[];
  live_overrides?: LiveOverride[];
  schema: FormSchema;
}

/** A widening change the caller may not make (403). */
export interface Refused {
  key: string;
  sentence: string;
  needs: string;
  command: string;
}

export interface Widened {
  key: string;
  sentence: string;
}

export interface SaveResult {
  saved: boolean;
  errors: string[];
  refused?: Refused[];
  widened?: Widened[];
  /** The digest of the config the answer judged against; send it back as `base`. */
  base?: string;
  /** On a dry run only: the config the save would write (for raw text, what it parses to). */
  preview?: Config;
  /** A 409 from a daemon started from flags: there is no file to write. */
  from_flags?: boolean;
  settings: SettingsView;
}

/** A TOML syntax error of the raw text, 1-based (POST /api/settings/raw answers 400 with it). */
export interface TomlError {
  line: number;
  column: number;
  message: string;
}

/** GET /api/settings/raw: the file's bytes as UTF-8 and the digest `base` checks. */
export interface RawFile {
  path: string;
  text: string;
  base: string;
}

/** An answer with the HTTP status it came with. */
export type Answered<T> = T & { status: number };

/** What a raw save or dry run answered: the form's `SaveResult`, or a TOML error. */
export type RawAnswer = Answered<SaveResult> | { status: number; tomlError: TomlError };
