import React from 'react';
import { GitBranch, Globe, BarChart3, Cpu, User, Settings, Trees } from 'lucide-react';

export type Page = 'repos' | 'network' | 'analytics' | 'compute' | 'profile' | 'settings';

const ITEMS: { id: Page; label: string; icon: React.ReactNode }[] = [
  { id: 'repos', label: 'Repos', icon: <GitBranch size={18} /> },
  { id: 'network', label: 'Network', icon: <Globe size={18} /> },
  { id: 'analytics', label: 'Analytics', icon: <BarChart3 size={18} /> },
  { id: 'compute', label: 'Compute', icon: <Cpu size={18} /> },
  { id: 'profile', label: 'Profile', icon: <User size={18} /> },
  { id: 'settings', label: 'Settings', icon: <Settings size={18} /> },
];

export default function Sidebar({ page, onNavigate }: { page: Page; onNavigate: (p: Page) => void }) {
  return (
    <div className="w-44 flex-shrink-0 theme-bg-secondary border-r theme-border flex flex-col">
      <div className="flex items-center gap-2 px-4 py-3 border-b theme-border">
        <Trees size={18} className="text-emerald-500" />
        <span className="text-sm font-semibold theme-text-primary">gitforest</span>
      </div>
      <nav className="flex-1 py-2">
        {ITEMS.map((item) => (
          <button
            key={item.id}
            onClick={() => onNavigate(item.id)}
            className={`w-full flex items-center gap-2.5 px-4 py-2.5 text-sm transition-colors ${
              page === item.id
                ? 'theme-bg-tertiary theme-text-primary border-r-2 border-emerald-500'
                : 'theme-text-muted hover:theme-bg-tertiary hover:theme-text-primary'
            }`}
          >
            {item.icon}
            {item.label}
          </button>
        ))}
      </nav>
    </div>
  );
}
