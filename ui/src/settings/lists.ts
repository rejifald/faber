import type { FieldSpec } from './types';

// What a `sudo` list or map may do in the form (anatomy.md § 2, "Trust lists narrow in place").
// The daemon refuses every widening anyway; this only keeps the form from offering one.

/** Whether an item may be added: only a list that narrows by adding. */
export function canAdd(field: FieldSpec): boolean {
  return !field.sudo ? true : field.narrows === 'add';
}

/** Whether the item at `index` of `count` may be removed. `remove-keep-one` keeps the last one. */
export function canRemove(field: FieldSpec, index: number, count: number): boolean {
  if (!field.sudo) return true;
  if (field.narrows === 'remove') return true;
  if (field.narrows === 'remove-keep-one') return count > 1 && index >= 0 && index < count;
  return false;
}

/** The error for a new item, or null when it is fine: the field's pattern first, then a duplicate. */
export function validateItem(field: FieldSpec, text: string, existing: string[]): string | null {
  const item = text.trim();
  if (field.item && !new RegExp(`^(?:${field.item.pattern})$`).test(item)) return field.item.error;
  if (existing.includes(item)) return `${item} is already in the list.`;
  return null;
}
