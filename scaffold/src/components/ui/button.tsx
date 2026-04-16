import { cn } from "@/lib/utils";

interface ButtonProps {
  children: React.ReactNode;
  variant?: "default" | "outline" | "ghost";
  className?: string;
  onClick?: () => void;
}

export function Button({
  children,
  variant = "default",
  className,
  onClick,
}: ButtonProps) {
  const baseStyles =
    "inline-flex items-center justify-center rounded-md text-sm font-medium transition-colors focus-visible:outline-none disabled:opacity-50";

  const variants = {
    default: "bg-slate-900 text-white hover:bg-slate-800",
    outline:
      "border border-slate-200 bg-white hover:bg-slate-100 hover:text-slate-900",
    ghost: "hover:bg-slate-100 hover:text-slate-900",
  };

  return (
    <button
      className={cn(baseStyles, variants[variant], className)}
      onClick={onClick}
    >
      {children}
    </button>
  );
}
