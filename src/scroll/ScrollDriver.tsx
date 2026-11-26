import { useEffect } from 'react';

/**
 * The self test's scroll content: a long, clearly striped page that scrolls
 * itself 150 px every second (instant, integer jumps), so the scroll-capture
 * loop has realistic content to stitch without any input injection. The rows
 * carry their offsets as text, which makes the stitched image easy to check.
 */
export function ScrollDriver() {
  useEffect(() => {
    let steps = 0;
    const timer = setInterval(() => {
      steps += 1;
      if (steps > 20) {
        clearInterval(timer);
        return;
      }
      window.scrollBy(0, 150);
    }, 1000);
    return () => clearInterval(timer);
  }, []);

  return (
    <div className="scroll-driver">
      {Array.from({ length: 60 }, (_, i) => (
        <div key={i} className="driver-row" style={{ background: i % 2 ? '#0f172a' : '#1e293b', color: i % 2 ? '#7dd3fc' : '#fef08a' }}>
          row {i} · {i * 80}px
        </div>
      ))}
    </div>
  );
}
