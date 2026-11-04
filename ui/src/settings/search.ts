import type { FieldSpec, FormSchema, SectionSpec } from './types';

export interface Hit {
  section: SectionSpec;
  field: FieldSpec;
}

/** The fields whose label, key or hint contain the query, case-insensitive; a blank query matches nothing. */
export function matches(query: string, schema: FormSchema): Hit[] {
  const q = query.trim().toLowerCase();
  if (!q) return [];
  const hits: Hit[] = [];
  for (const section of schema.sections) {
    for (const field of section.fields) {
      if ([field.label, field.key, field.hint ?? ''].some((s) => s.toLowerCase().includes(q))) hits.push({ section, field });
    }
  }
  return hits;
}
