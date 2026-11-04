import { userEvent, waitFor } from 'storybook/test';

// What a story's play function does to reach an open state. Native events, not userEvent's
// pointer simulation: Base UI's Menu treats userEvent's synthetic press as open-then-close, and
// userEvent.tab walks into Storybook's own hidden error page. Nor focus: a story browser in a
// background window has no focus to give, so `:focus-visible` and focus-opened tooltips never
// happen there; the focus-visible stories force the ring instead (parameters.force).

type Context = { canvasElement: HTMLElement };

const trigger = ({ canvasElement }: Context, selector: string) => {
  const found = canvasElement.querySelector<HTMLElement>(selector);
  if (!found) throw new Error(`no ${selector} in the story`);
  return found;
};

/** Waits until a popup matching `selector` is in the page (popups render in a portal). */
const opened = (selector: string) =>
  waitFor(() => {
    if (!document.querySelector(selector)) throw new Error(`${selector} did not open`);
  });

/** Clicks the story's first `selector`, then waits for what it shows (`popup`, often in a portal). */
export async function click(context: Context, selector: string, popup: string) {
  trigger(context, selector).click();
  await opened(popup);
}

/** Moves the pointer onto the story's first `selector`, then waits for its tooltip. */
export async function hover(context: Context, selector: string) {
  const target = trigger(context, selector);
  const at = { clientX: 1, clientY: 1, pointerType: 'mouse' };
  target.dispatchEvent(new PointerEvent('pointerenter', at));
  target.dispatchEvent(new MouseEvent('mouseenter', at));
  target.dispatchEvent(new PointerEvent('pointermove', { ...at, bubbles: true }));
  target.dispatchEvent(new MouseEvent('mousemove', { ...at, bubbles: true }));
  await opened('[data-open][data-side] > *');
}

/** Presses keys in the focused element: an open popup takes focus, so this moves its highlight. */
export const keys = (sequence: string) => userEvent.keyboard(sequence);
