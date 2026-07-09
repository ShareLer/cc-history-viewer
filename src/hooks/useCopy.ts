import { useCallback, useEffect, useRef, useState } from "react";

/**
 * 复制到剪贴板 + 短暂「已复制」反馈。
 * copy(text) 成功后 copied 变为 true，约 resetDelay 毫秒后自动复原，
 * 并返回 true；失败时返回 false。
 * 供按钮把 Copy 图标短暂切换为 Check。
 */
export function useCopy(resetDelay = 1500) {
  const [copied, setCopied] = useState(false);
  const timerRef = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) window.clearTimeout(timerRef.current);
    };
  }, []);

  const copy = useCallback(
    async (text: string) => {
      try {
        await navigator.clipboard.writeText(text);
        setCopied(true);
        if (timerRef.current !== null) window.clearTimeout(timerRef.current);
        timerRef.current = window.setTimeout(
          () => setCopied(false),
          resetDelay
        );
        return true;
      } catch {
        return false;
      }
    },
    [resetDelay]
  );

  return { copied, copy };
}
