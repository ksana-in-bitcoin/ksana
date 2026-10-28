/** The source editor: CodeMirror 6 with Rust highlighting and the compiler's diagnostics underlined in place. */
import { useEffect, useRef } from 'react';
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { rust } from '@codemirror/lang-rust';
import { HighlightStyle, bracketMatching, foldGutter, indentOnInput, syntaxHighlighting } from '@codemirror/language';
import { lintGutter, setDiagnostics, type Diagnostic as CmDiagnostic } from '@codemirror/lint';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { Compartment, EditorState, type Text } from '@codemirror/state';
import {
  EditorView, crosshairCursor, drawSelection, highlightActiveLine, highlightActiveLineGutter, highlightSpecialChars, keymap,
  lineNumbers, rectangularSelection,
} from '@codemirror/view';
import { tags } from '@lezer/highlight';
import type { Diagnostic } from '../compile/client';

const highlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.controlKeyword, tags.moduleKeyword, tags.definitionKeyword, tags.modifier], color: 'var(--code-keyword)' },
  { tag: [tags.string, tags.special(tags.string), tags.character], color: 'var(--code-string)' },
  { tag: [tags.number, tags.bool, tags.atom], color: 'var(--code-number)' },
  { tag: [tags.lineComment, tags.blockComment, tags.docComment], color: 'var(--code-comment)', fontStyle: 'italic' },
  { tag: [tags.typeName, tags.className, tags.namespace], color: 'var(--code-type)' },
  { tag: [tags.function(tags.variableName), tags.function(tags.propertyName), tags.macroName], color: 'var(--code-function)' },
  { tag: [tags.attributeName, tags.meta], color: 'var(--code-meta)' },
  { tag: tags.invalid, color: 'var(--danger)' },
]);

const theme = EditorView.theme({
  '&': { height: '100%', fontSize: '13px', backgroundColor: 'var(--surface)', color: 'var(--ink)' },
  '.cm-scroller': { fontFamily: 'var(--font-mono)', lineHeight: '1.55' },
  '.cm-content': { caretColor: 'var(--ink)', padding: '8px 0' },
  '.cm-gutters': { backgroundColor: 'var(--surface-2)', color: 'var(--ink-4)', borderRight: '1px solid var(--line)' },
  '.cm-activeLine': { backgroundColor: 'color-mix(in srgb, var(--qing-soft) 45%, transparent)' },
  '.cm-activeLineGutter': { backgroundColor: 'var(--qing-soft)', color: 'var(--ink-2)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': { backgroundColor: 'color-mix(in srgb, var(--qing) 28%, transparent) !important' },
  '.cm-cursor': { borderLeftColor: 'var(--ink)' },
  '.cm-matchingBracket': { backgroundColor: 'var(--qing-soft)', outline: '1px solid var(--qing-line)' },
  '.cm-tooltip': { backgroundColor: 'var(--surface)', border: '1px solid var(--line-strong)', color: 'var(--ink)' },
  '.cm-diagnostic': { fontFamily: 'var(--font-mono)', fontSize: '12px', whiteSpace: 'pre-wrap' },
  '.cm-panels': { backgroundColor: 'var(--surface-2)', color: 'var(--ink)' },
});

/** The offset of a 1-based line and column (rustc counts columns in characters) */
function offsetOf(doc: Text, line: number, column: number): number {
  const l = doc.line(Math.min(Math.max(line, 1), doc.lines));
  return Math.min(l.from + Math.max(column - 1, 0), l.to);
}

export function toCmDiagnostics(doc: Text, diagnostics: Diagnostic[]): CmDiagnostic[] {
  const out: CmDiagnostic[] = [];
  for (const d of diagnostics) {
    const span = d.spans.find((s) => s.primary) ?? d.spans[0];
    if (!span) continue;
    const from = offsetOf(doc, span.lineStart, span.columnStart);
    let to = offsetOf(doc, span.lineEnd, span.columnEnd);
    if (to <= from) to = Math.min(from + 1, doc.length);
    out.push({
      from,
      to,
      severity: d.level === 'error' ? 'error' : d.level === 'warning' ? 'warning' : 'info',
      message: span.label ? `${d.message}\n${span.label}` : d.message,
    });
  }
  return out;
}

export interface EditorProps {
  value: string;
  onChange: (value: string) => void;
  diagnostics: Diagnostic[];
  readOnly?: boolean;
  /** A line to show and put the cursor on (from a diagnostic) */
  goto?: { line: number; n: number } | null;
  label: string;
}

export function Editor({ value, onChange, diagnostics, readOnly = false, goto, label }: EditorProps) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const readOnlyConf = useRef(new Compartment());

  useEffect(() => {
    if (!host.current) return;
    const v = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: value,
        extensions: [
          lineNumbers(), highlightActiveLineGutter(), highlightSpecialChars(), history(), foldGutter(), drawSelection(),
          EditorState.allowMultipleSelections.of(true), indentOnInput(), syntaxHighlighting(highlight), bracketMatching(),
          closeBrackets(), rectangularSelection(), crosshairCursor(), highlightActiveLine(), highlightSelectionMatches(),
          keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, indentWithTab]),
          rust(), lintGutter(), theme, EditorState.tabSize.of(4),
          readOnlyConf.current.of(EditorState.readOnly.of(readOnly)),
          EditorView.contentAttributes.of({ 'aria-label': label }),
          EditorView.updateListener.of((u) => {
            if (u.docChanged) onChangeRef.current(u.state.doc.toString());
          }),
        ],
      }),
    });
    view.current = v;
    return () => {
      v.destroy();
      view.current = null;
    };
    // the editor is created once; value changes from outside are applied below
  }, []);

  useEffect(() => {
    const v = view.current;
    if (!v) return;
    const current = v.state.doc.toString();
    if (current !== value) v.dispatch({ changes: { from: 0, to: current.length, insert: value } });
  }, [value]);

  useEffect(() => {
    const v = view.current;
    if (!v) return;
    v.dispatch({ effects: readOnlyConf.current.reconfigure(EditorState.readOnly.of(readOnly)) });
  }, [readOnly]);

  useEffect(() => {
    const v = view.current;
    if (!v) return;
    v.dispatch(setDiagnostics(v.state, toCmDiagnostics(v.state.doc, diagnostics)));
  }, [diagnostics, value]);

  useEffect(() => {
    const v = view.current;
    if (!v || !goto) return;
    const pos = offsetOf(v.state.doc, goto.line, 1);
    v.dispatch({ selection: { anchor: pos }, effects: EditorView.scrollIntoView(pos, { y: 'center' }) });
    v.focus();
  }, [goto]);

  return <div className="editor" ref={host} />;
}
