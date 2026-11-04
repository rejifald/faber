import '../theme/theme.css';
import { StrictMode, type ReactNode } from 'react';
import { createRoot } from 'react-dom/client';
import { TooltipProvider } from '../kit';

/** Renders an entry's root into `#root`, inside the providers every surface shares. */
export function mount(app: ReactNode) {
  const root = document.getElementById('root');
  if (!root) throw new Error('no #root element in the entry page');
  createRoot(root).render(
    <StrictMode>
      <TooltipProvider>{app}</TooltipProvider>
    </StrictMode>,
  );
}
