interface Props {
  count: number;
  onClick?: () => void;
}

export function OverflowCounter({ count, onClick }: Props) {
  return (
    <div
      onMouseDown={(e) => e.stopPropagation()}
      onClick={onClick}
      onMouseEnter={(e) => {
            (e.currentTarget as HTMLDivElement).style.background = "var(--bg-surface-hover)";
            (e.currentTarget as HTMLDivElement).style.color = "var(--text-primary)";
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLDivElement).style.background = "var(--bg-surface)";
            (e.currentTarget as HTMLDivElement).style.color = "var(--text-secondary)";
          }}
      style={{
        cursor: onClick ? "pointer" : "default",
        fontSize: 12,
        fontWeight: 600,
        color: "var(--text-secondary)",
        background: "var(--bg-surface)",
        borderRadius: 6,
        padding: "2px 6px",
        alignSelf: "center",
        userSelect: "none",
      }}
    >
      +{count}
    </div>
  );
}
