import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowUp, FileText, Folder, Home, RefreshCw } from 'lucide-react';

type Scope = 'hosted' | 'network';

interface Props {
  scope: Scope;
  repoName: string;
  repoKey: string;
  owner?: string | null;
  headSha?: string | null;
}

function formatSize(size: unknown): string {
  const n = typeof size === 'number' ? size : Number(size || 0);
  if (!Number.isFinite(n) || n <= 0) return '';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

export default function RepoFiles({ scope, repoName, repoKey, owner, headSha }: Props) {
  const [path, setPath] = useState('');
  const [entries, setEntries] = useState<any[]>([]);
  const [commit, setCommit] = useState('');
  const [source, setSource] = useState('');
  const [treeLoading, setTreeLoading] = useState(false);
  const [treeError, setTreeError] = useState<string | null>(null);
  const [file, setFile] = useState<any | null>(null);
  const [fileLoading, setFileLoading] = useState(false);
  const [fileError, setFileError] = useState<string | null>(null);
  const requestRef = useRef(0);

  const route = useCallback((kind: 'tree' | 'file', filePath: string) => {
    const base = scope === 'hosted'
      ? `/api/repos/${encodeURIComponent(repoName)}`
      : `/api/network/repos/${encodeURIComponent(repoName)}`;
    const params = new URLSearchParams({ ref: 'HEAD', path: filePath });
    if (scope === 'network') {
      if (headSha) params.set('head_sha', headSha);
      if (owner) params.set('owner', owner);
    }
    return `${base}/${kind}?${params.toString()}`;
  }, [scope, repoName, headSha, owner]);

  const loadTree = useCallback(async (nextPath: string) => {
    const request = ++requestRef.current;
    setTreeLoading(true);
    setTreeError(null);
    try {
      const res: any = await window.gf.api(route('tree', nextPath));
      if (request !== requestRef.current) return;
      if (!res.ok) {
        setEntries([]);
        setTreeError(res.data?.error || res.error || `Could not list files (HTTP ${res.status})`);
        return;
      }
      setPath(String(res.data?.path || nextPath));
      setEntries(Array.isArray(res.data?.entries) ? res.data.entries : []);
      setCommit(String(res.data?.commit || ''));
      const src = res.data?.source_endpoint
        ? `${res.data.source_mode || 'remote'} · ${res.data.source_endpoint}`
        : '';
      setSource(src);
    } catch (e) {
      if (request === requestRef.current) setTreeError((e as Error).message);
    } finally {
      if (request === requestRef.current) setTreeLoading(false);
    }
  }, [route]);

  const loadFile = useCallback(async (filePath: string) => {
    setFileLoading(true);
    setFileError(null);
    setFile(null);
    try {
      const res: any = await window.gf.api(route('file', filePath));
      if (!res.ok) {
        setFileError(res.data?.error || res.error || `Could not read file (HTTP ${res.status})`);
        return;
      }
      setFile(res.data);
    } catch (e) {
      setFileError((e as Error).message);
    } finally {
      setFileLoading(false);
    }
  }, [route]);

  useEffect(() => {
    setPath('');
    setEntries([]);
    setFile(null);
    setFileError(null);
    loadTree('');
  }, [repoKey, loadTree]);

  const parts = path ? path.split('/').filter(Boolean) : [];
  const parentPath = parts.length ? parts.slice(0, -1).join('/') : '';

  const openEntry = (entry: any) => {
    if (entry.type === 'tree') {
      setFile(null);
      setFileError(null);
      loadTree(String(entry.path || ''));
    } else if (entry.type === 'blob') {
      loadFile(String(entry.path || ''));
    }
  };

  return (
    <div className="theme-bg-secondary rounded border theme-border mb-6">
      <div className="flex items-center justify-between gap-3 px-4 py-3 border-b theme-border">
        <div className="min-w-0">
          <h3 className="text-sm font-semibold theme-text-primary">Files</h3>
          <p className="text-[11px] theme-text-muted truncate">
            HEAD{commit ? ` · ${commit.slice(0, 10)}` : ''}{source ? ` · ${source}` : ''}{scope === 'network' ? ' · read-only' : ''}
          </p>
        </div>
        <button onClick={() => loadTree(path)} className="p-1.5 theme-text-muted hover:theme-text-primary" title="Refresh files">
          <RefreshCw size={14} className={treeLoading ? 'animate-spin' : ''} />
        </button>
      </div>

      <div className="flex items-center gap-1 px-4 py-2 border-b theme-border text-xs overflow-x-auto">
        <button onClick={() => { setFile(null); loadTree(''); }} className="flex items-center gap-1 theme-text-secondary hover:theme-text-primary flex-shrink-0">
          <Home size={12} /> root
        </button>
        {parts.map((part, i) => {
          const target = parts.slice(0, i + 1).join('/');
          return (
            <React.Fragment key={target}>
              <span className="theme-text-muted">/</span>
              <button onClick={() => { setFile(null); loadTree(target); }} className="theme-text-secondary hover:theme-text-primary flex-shrink-0">{part}</button>
            </React.Fragment>
          );
        })}
        {path && (
          <button onClick={() => { setFile(null); loadTree(parentPath); }} className="ml-auto flex items-center gap-1 theme-text-muted hover:theme-text-primary flex-shrink-0">
            <ArrowUp size={12} /> Up
          </button>
        )}
      </div>

      <div className="grid lg:grid-cols-[300px_1fr] min-h-64">
        <div className="border-b lg:border-b-0 lg:border-r theme-border overflow-auto max-h-[560px]">
          {treeLoading && <p className="text-xs theme-text-muted p-4">Loading files…</p>}
          {treeError && <p className="text-xs text-red-500 p-4">{treeError}</p>}
          {!treeLoading && !treeError && entries.length === 0 && <p className="text-xs theme-text-muted p-4">No files at this level.</p>}
          {entries.map((entry: any) => (
            <button key={entry.path} onClick={() => openEntry(entry)}
              className={`w-full flex items-center justify-between gap-2 px-4 py-2 text-left border-b theme-border last:border-0 hover:theme-bg-tertiary ${file?.path === entry.path ? 'theme-bg-tertiary' : ''}`}>
              <span className="flex items-center gap-2 min-w-0">
                {entry.type === 'tree'
                  ? <Folder size={14} className="text-emerald-500 flex-shrink-0" />
                  : <FileText size={14} className="theme-text-muted flex-shrink-0" />}
                <span className="text-xs theme-text-primary truncate">{entry.name}</span>
              </span>
              <span className="text-[11px] theme-text-muted flex-shrink-0">{entry.type === 'blob' ? formatSize(entry.size) : ''}</span>
            </button>
          ))}
        </div>

        <div className="min-w-0 flex flex-col min-h-64">
          {!file && !fileLoading && !fileError && (
            <div className="flex-1 flex items-center justify-center p-6 text-xs theme-text-muted">Select a file to read it.</div>
          )}
          {fileLoading && <div className="flex-1 flex items-center justify-center p-6 text-xs theme-text-muted">Reading file…</div>}
          {fileError && <p className="text-xs text-red-500 p-4">{fileError}</p>}
          {file && (
            <>
              <div className="flex items-center justify-between gap-3 px-4 py-2 border-b theme-border">
                <code className="text-xs theme-text-primary truncate">{file.path}</code>
                <span className="text-[11px] theme-text-muted flex-shrink-0">
                  {formatSize(file.size)}{file.truncated ? ' · preview truncated' : ''}
                </span>
              </div>
              {file.binary ? (
                <div className="flex-1 flex items-center justify-center p-6 text-xs theme-text-muted">
                  {file.too_large ? 'This file is too large to preview.' : 'Binary file — preview is not available.'}
                </div>
              ) : (
                <pre className="flex-1 overflow-auto max-h-[520px] p-4 text-[12px] leading-5 font-mono theme-text-secondary whitespace-pre">{file.content || ''}</pre>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
