interface Props {
  onClick?: () => void;
}

export function ListBadge({ onClick }: Props) {
  return (
    <div
      onMouseDown={(e) => e.stopPropagation()}
      onClick={onClick}
      style={{
        cursor: onClick ? "pointer" : "default",
        fontSize: 14,
        color: "var(--text-secondary)",
        background: "var(--bg-surface)",
        borderRadius: 6,
        padding: "2px 6px",
        alignSelf: "center",
        userSelect: "none",
        lineHeight: 1,
      }}
    >
      ≡
    </div>
  );
}
