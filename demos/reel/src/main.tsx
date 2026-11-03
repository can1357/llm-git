import React, { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import "./styles.css";
import wallpaper from "./assets/sample-wallpaper.svg";

type Decision = "approved" | "rejected";
type Signal = { tag: string; detail: string };
type Analysis = { tier: "focus" | "review" | "ezpz"; score: number; signals: Signal[] };
type Hunk = { id: string; header: string; lines: string[]; truncated: boolean; vetoed: boolean; approved: boolean; analysis: Analysis };
type FileRec = { path: string; status: string; adds: number; dels: number; binary: boolean; hunks: Hunk[] };
type Chapter = { id: string; title: string; heuristicTitle: string; reason: string; gist: string; adds: number; dels: number; decision: Decision | null; files: string[]; fileCount: number; attention: number; counts: { focus: number; review: number; ezpz: number }; topSignal: (Signal & { tier: string }) | null };
type Current = Omit<Chapter, "files"> & { files: FileRec[]; briefing: string[] };
type State = { sample: boolean; shotMode: boolean; ai: { status: "loading" | "mapping" | "reducing" | "ready" | "offline"; narrative: string | null; mapped?: number; total?: number }; repo: string; currentIndex: number; total: number; popupOpen: boolean; canUndo: boolean; current: Current | null; chapters: Chapter[]; recap: { approved: Chapter[]; rejected: Chapter[] }; autoSkim: { hunks: number; files: number }; canApply: boolean; applyResult: { staged: number; errors: string[] } | null };
type Action = { type: "decide"; decision: Decision } | { type: "navigate"; index: number } | { type: "acceptEzpz" } | { type: "undo" } | { type: "veto"; hunkId: string } | { type: "popup"; open?: boolean } | { type: "apply" } | { type: "reload" } | { type: "quit" };
declare global { interface Window { reel: { getState: () => Promise<State>; onState: (cb: (state: State) => void) => void; act: (action: Action) => void } } }

function useReelState() {
  const [state, setState] = useState<State | null>(null);
  useEffect(() => { window.reel.getState().then(setState); window.reel.onState(setState); }, []);
  return state;
}
const act = (action: Action) => window.reel.act(action);

function useKeys(state: State | null) {
  useEffect(() => {
    if (!state) return;
    const onKey = (event: KeyboardEvent) => {
      const key = event.key.toLowerCase();
      if (key === "j" || event.key === "ArrowDown") act({ type: "navigate", index: state.currentIndex + 1 });
      else if (key === "k" || event.key === "ArrowUp") act({ type: "navigate", index: state.currentIndex - 1 });
      else if (event.key === "ArrowLeft") act({ type: "decide", decision: "rejected" });
      else if (event.key === "ArrowRight") act({ type: "decide", decision: "approved" });
      else if (key === "a") act({ type: "acceptEzpz" });
      else if (event.key === "Enter") act({ type: "popup" });
      else if (event.key === "Escape" && state.popupOpen) act({ type: "popup", open: false });
      else if (key === "u" || (key === "z" && event.metaKey)) act({ type: "undo" });
      else if (key === "q" && event.metaKey) act({ type: "quit" });
      else return;
      event.preventDefault();
    };
    addEventListener("keydown", onKey);
    return () => removeEventListener("keydown", onKey);
  }, [state]);
}

function Card() {
  const state = useReelState();
  const feed = useRef<HTMLDivElement>(null);
  const lastSent = useRef(-1);
  useKeys(state);
  useEffect(() => {
    if (!state || !feed.current) return;
    const page = feed.current.children[state.currentIndex] as HTMLElement | undefined;
        if (page && Math.abs(feed.current.scrollTop - page.offsetTop) > 8) feed.current.scrollTo({ top: page.offsetTop, behavior: state.shotMode ? "auto" : "smooth" });
  }, [state?.currentIndex, state?.shotMode]);
  useEffect(() => {
    const root = feed.current;
    if (!root) return;
    let timer = 0;
    const onScroll = () => {
      clearTimeout(timer);
      timer = window.setTimeout(() => {
        const index = Math.round(root.scrollTop / root.clientHeight);
        if (index !== lastSent.current) {
          lastSent.current = index;
          act({ type: "navigate", index });
        }
      }, 120);
    };
    root.addEventListener("scroll", onScroll, { passive: true });
    return () => {
      clearTimeout(timer);
      root.removeEventListener("scroll", onScroll);
    };
  }, [state?.chapters.length]);
  if (!state) return null;
  const shotStyle = state.shotMode ? { "--shot-wallpaper": `url(${wallpaper})` } as React.CSSProperties : undefined;
    return <div className={`shell ai-${state.ai.status} ${state.shotMode ? "shot-mode" : ""}`} style={shotStyle}>
    <header className="drag"><button className="close" aria-label="Quit Reel" onClick={() => act({ type: "quit" })}/><span>reel</span><span className="repo">{state.repo}</span>{state.sample && <span className="sample">sample</span>}</header>
    <div className="feed" ref={feed}>{state.chapters.map((chapter, index) => <article className={`chapter ${index === state.currentIndex ? "current " : ""}${chapter.decision || ""}`} data-index={index} key={chapter.id}>
      <div className="chapter-no"><span>{String(index + 1).padStart(2, "0")}</span><span>{index + 1} / {state.total}</span></div>
      <div className="decision-mark" aria-label={chapter.decision || "undecided"}><svg viewBox="0 0 24 24" aria-hidden="true"><path className="check-path" d="m6 12 4 4 8-9"/><path className="cross-path" d="m8 8 8 8"/><path className="cross-path" d="m16 8-8 8"/></svg></div>
            <h1>{chapter.title}</h1>
      <div className="attention"><i style={{ width: `${Math.round(chapter.attention * 100)}%` }}/></div>
      <p className={`reason ${chapter.topSignal?.tier || ""}`}><i/>{chapter.topSignal?.detail || chapter.reason}</p>
            <div className="tier-counts"><span className="focus">● focus {chapter.counts.focus}</span><span>review {chapter.counts.review}</span><span className="ezpz">ezpz {chapter.counts.ezpz}</span></div>
      <p className="gist">{chapter.gist}</p>
      <div className="files">{chapter.files.map((file) => <span key={file}>{file.split("/").pop()}</span>)}{chapter.fileCount > chapter.files.length && <span className="more-files">+{chapter.fileCount - chapter.files.length} more</span>}</div>
            <div className="peek"><span className="plus">+{chapter.adds}</span><span className="minus">−{chapter.dels}</span><button onClick={() => act({ type: "popup" })}>{state.popupOpen ? "Hide full diff" : "Show full diff"} <kbd>⏎</kbd></button></div>
      <div className="decision-buttons"><button onClick={() => act({ type: "decide", decision: "rejected" })}><kbd>←</kbd> reject</button><button onClick={() => act({ type: "decide", decision: "approved" })}>approve <kbd>→</kbd></button></div>
    </article>)}</div>
    <nav className="dots" aria-label="Chapters">{state.chapters.map((chapter, index) => <button key={chapter.id} aria-label={`Chapter ${index + 1}`} className={`${index === state.currentIndex ? "active " : ""}${chapter.decision || ""}`} onClick={() => act({ type: "navigate", index })}><i/></button>)}</nav>
            <footer><span><kbd>J</kbd><kbd>K</kbd> · <kbd>A</kbd> skim</span><span className={`ai-status ${state.ai.status}`}>{state.ai.status === "ready" ? "✦ ai" : state.ai.status === "mapping" ? <><i/> ✦ ai · mapping {state.ai.mapped}/{state.ai.total}</> : state.ai.status === "reducing" ? <><i/> ✦ ai · reducing…</> : state.ai.status === "loading" ? <><i/> ✦ ai</> : "ai offline"}</span><span>{state.autoSkim.hunks ? `${state.autoSkim.hunks} skimmed` : state.canUndo ? <><kbd>U</kbd> undo</> : "scroll to review"}</span></footer>
  </div>;
}

function Popup() {
  const state = useReelState();
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  useKeys(state);
  if (!state?.current) return null;
  const current = state.current;
  const shotStyle = state.shotMode ? { "--shot-wallpaper": `url(${wallpaper})` } as React.CSSProperties : undefined;
    return <div className={`popup ai-${state.ai.status} ${state.shotMode ? "shot-mode" : ""}`} style={shotStyle}>
    <header className="popup-drag">
            <div className="popup-heading"><span>Chapter {state.currentIndex + 1} · attention {Math.round(current.attention * 100)}</span><h1>{current.title}</h1>{current.briefing.length > 0 && <div className="briefing"><b>✦</b><div><span className="briefing-label">Reviewer briefing</span>{current.briefing.map((note) => <p key={note}>{note}</p>)}</div></div>}</div>
      <button aria-label="Close details" onClick={() => act({ type: "popup", open: false })}>×</button>
    </header>
    <div className="diff" key={current.id}>{current.files.map((file, fileIndex) => <section className="file" key={file.path} style={{ "--file-i": Math.min(fileIndex, 7) } as React.CSSProperties}>
      <div className="file-head"><span>{file.path}</span><span className="stats">+{file.adds} −{file.dels}</span></div>
      {file.binary ? <p className="binary">Binary file</p> : file.hunks.map((hunk, hunkIndex) => {
        const collapsed = hunk.analysis.tier === "ezpz" && !expanded.has(hunk.id);
        const groupedSignals = hunk.analysis.signals.reduce<{ signal: Signal; count: number }[]>((groups, signal) => {
          const existing = groups.find((group) => group.signal.tag === signal.tag);
          if (existing) existing.count++;
          else groups.push({ signal, count: 1 });
          return groups;
        }, []);
        const visibleSignals = groupedSignals.slice(0, 4);
        if (collapsed) return <button className={`collapsed-hunk ${hunk.approved ? "approved" : ""}`} key={hunk.id} style={{ "--hunk-i": Math.min(hunkIndex, 8) } as React.CSSProperties} onClick={() => setExpanded((prior) => new Set(prior).add(hunk.id))}>
          <span className="signal-chip ezpz">{hunk.analysis.signals[0]?.tag || "trivial"}</span>
          <span>{hunk.lines.length} lines — click to expand</span>
          {hunk.approved && <strong>skimmed ✓</strong>}
        </button>;
        return <div className={hunk.vetoed ? "hunk vetoed" : "hunk"} key={hunk.id}>
                    {visibleSignals.length > 0 && <div className="signal-row">{visibleSignals.map(({ signal, count }) => <span className={`signal-chip ${hunk.analysis.tier}`} title={signal.detail} key={signal.tag}>{signal.tag}{count > 1 ? ` ×${count}` : ""}</span>)}{groupedSignals.length > 4 && <span className="signal-chip review">+{groupedSignals.length - 4} more</span>}</div>}
          <button className="hunk-head" onClick={() => act({ type: "veto", hunkId: hunk.id })}><code>{hunk.header}</code><span>{hunk.vetoed ? "include" : "exclude"}</span></button>
          {hunk.lines.map((line, index) => <div className={`line ${line.startsWith("+") ? "add" : line.startsWith("-") ? "del" : ""}`} key={index}><span>{line.startsWith("+") ? "+" : line.startsWith("-") ? "−" : " "}</span><code>{line.slice(line.startsWith("+") || line.startsWith("-") ? 1 : 0) || " "}</code></div>)}
        </div>;
      })}
    </section>)}</div>
    <footer className="popup-footer">
      <span>{state.autoSkim.hunks ? `auto-skimmed ${state.autoSkim.hunks} trivial hunks across ${state.autoSkim.files} files` : "Focus signals stay expanded · ezpz stays folded"}</span>
      <div>{(current.id === "chores" || current.counts.ezpz > 0) && <button onClick={() => act({ type: "acceptEzpz" })}><kbd>A</kbd> {current.id === "chores" ? "Accept chores" : "Accept ezpz"}</button>}<button onClick={() => act({ type: "decide", decision: "rejected" })}>Reject</button><button className="approve" onClick={() => act({ type: "decide", decision: "approved" })}>{current.decision === "approved" ? "Approved ✓" : "Approve"}</button></div>
    </footer>
  </div>;
}

createRoot(document.getElementById("root")!).render(location.hash === "#popup" ? <Popup/> : <Card/>);
