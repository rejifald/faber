import type { Meta, StoryObj } from '@storybook/react-vite';
import { useState } from 'react';
import { click } from '../../.storybook/play';
import { Button } from './Button';
import { Dialog } from './Dialog';
import { Field, Input } from './Field';

// Dialog's @states (closed, open, submitting, error). The dialog opens in a play function, from
// the trigger a screen would press.

interface HarnessProps {
  submitting?: boolean;
  error?: string;
}

function Harness({ submitting = false, error }: HarnessProps) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button onClick={() => setOpen(true)}>Hold Docker</Button>
      <Dialog
        open={open}
        onOpenChange={setOpen}
        title="Hold Docker"
        description="No new jobs start while the hold is set."
        error={error}
        actions={
          <>
            <Button onClick={() => setOpen(false)}>Cancel</Button>
            <Button variant="primary" loading={submitting}>
              Hold
            </Button>
          </>
        }
      >
        <Field label="Reason">
          <Input defaultValue="Resizing the VM" />
        </Field>
      </Dialog>
    </>
  );
}

const meta = { title: 'Kit/Dialog', component: Harness } satisfies Meta<typeof Harness>;
export default meta;
type Story = StoryObj<typeof meta>;

const open = (context: Parameters<typeof click>[0]) => click(context, 'button', '[role="dialog"]');

export const Closed: Story = {};
export const Open: Story = { play: open };
export const Submitting: Story = { args: { submitting: true }, play: open };
export const Failed: Story = { args: { error: 'The daemon refused the hold: not the owner.' }, play: open };
