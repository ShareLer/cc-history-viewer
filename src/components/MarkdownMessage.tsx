import ReactMarkdown from "react-markdown";
import rehypeHighlight from "rehype-highlight";
import remarkGfm from "remark-gfm";

import { cn } from "@/lib/utils";

export function MarkdownMessage({
  text,
  className,
}: {
  text: string;
  className?: string;
}) {
  return (
    <div className={cn("markdown-message", className)}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[[rehypeHighlight, { ignoreMissing: true }]]}
        components={{
          a: ({ node: _node, ...props }) => (
            <a target="_blank" rel="noreferrer" {...props} />
          ),
        }}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
}
