import { DashboardApp } from '../src/entries/DashboardApp';
import { mount } from '../src/entries/mount';
import { Settings } from '../src/screens/Settings';

// The daemon serves this shell at `/`, `/activity`, `/memory`, `/maintenance` and `/settings` (ADR-0054 § 5,
// ADR-0028 addendum C). Settings is its own page; the rest are the dashboard's views.
mount(location.pathname === '/settings' ? <Settings /> : <DashboardApp />);
