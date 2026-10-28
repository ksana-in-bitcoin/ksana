/** `#/ai`: the contract reference for AI assistants to copy, download or read, how to use it, and prompts to start from. */
import { useEffect, useState } from 'react';
import { useT } from '../i18n';
import { Banner, Button, CopyButton, Section, copyText, saveFile } from '../ui/ui';
import { REFERENCE, REFERENCE_FILE, REFERENCE_URL, referenceKb, referenceSections } from './reference';

export function AiPage() {
  const m = useT().core.ai;
  /** null before a copy, then whether the browser took it */
  const [copied, setCopied] = useState<boolean | null>(null);
  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(null), 2_000);
    return () => clearTimeout(timer);
  }, [copied]);
  const steps = ['copy', 'ask', 'compile', 'deploy'] as const;
  return <div className="home">
    <section className="hero">
      <h1>{m.title}</h1>
      <p className="lead">{m.lead}</p>
      <div className="hero-actions">
        <Button variant="primary" onClick={() => void copyText(REFERENCE).then(setCopied)}>{copied ? m.copied : m.copy}</Button>
        <Button onClick={() => saveFile(REFERENCE_FILE, REFERENCE, 'text/markdown')}>{m.download}</Button>
        <a className="btn btn--ghost btn--md" href={REFERENCE_URL} target="_blank" rel="noreferrer">{m.asText}</a>
      </div>
      <p className="field-hint hero-note">{m.size(referenceKb())}</p>
      {copied === false && <Banner tone="warn">{m.copyFailed}</Banner>}
    </section>
    <Section title={m.stepsTitle}>
      <ol className="steps">
        {steps.map((s, i) => <li key={s}><span className="step-n">{i + 1}</span><div><h3>{m.steps[s].title}</h3><p>{m.steps[s].body}</p></div></li>)}
      </ol>
    </Section>
    <Section title={m.promptsTitle}>
      <p className="field-hint">{m.promptsHint}</p>
      <ul className="prompts">
        {m.prompts.map((p) => <li key={p} className="prompt"><p>{p}</p><CopyButton text={p} /></li>)}
      </ul>
    </Section>
    <Section title={m.contentsTitle}>
      <ul className="contents">{referenceSections().map((s) => <li key={s}>{s}</li>)}</ul>
      <details className="reference">
        <summary>{m.show}</summary>
        <pre className="reference-text">{REFERENCE}</pre>
      </details>
    </Section>
  </div>;
}
