# AlBDO Project

A high-performance JSX/TSX application built with AlBDO.

## No Node.js in Production

AlBDO compiles your components to **pure Rust** binaries. There is zero Node.js in the production runtime.

| Environment | Node.js Required? |
|-------------|-------------------|
| Development (`npm run dev`) | Yes - for HMR server |
| Build (`npm run build`) | Yes - runs AlBDO CLI |
| **Production** | **No - pure Rust binary** |

Your production server runs the AlBDO Rust binary directly.

## Getting Started

```bash
# Install AlBDO CLI (requires Node.js - but only for this step)
npm install -g albedo

# Create new project
albedo init my-app
cd my-app

# Development (Node.js + AlBDO)
npm install
npm run dev

# Build for production
npm run build

# Ship (deploys Rust binary - no Node.js needed)
npm run ship
```

## Using External Components

AlBDO works with any React component library.

### Shadcn/ui

```bash
# Initialize shadcn/ui
npx shadcn-ui@latest init

# Add components
npx shadcn-ui@latest add button card
```

```tsx
import { Button } from "@/components/ui/button";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";

export default function App() {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Hello</CardTitle>
      </CardHeader>
      <CardContent>
        <Button>Click me</Button>
      </CardContent>
    </Card>
  );
}
```

### Other Libraries

```bash
npm install @mui/material framer-motion lucide-react
```

```tsx
import { Button } from "@mui/material";
import { motion } from "framer-motion";
import { Moon } from "lucide-react";
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     DEVELOPMENT                              │
│  Node.js + AlBDO CLI                                        │
│  • HMR over SSE                                             │
│  • Component analysis                                        │
│  • Hot reload                                                │
└─────────────────────────────────────────────────────────────┘
                              │
                              │ albedo build
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                     PRODUCTION                               │
│  Pure Rust Binary (albedo serve)                           │
│  • Zero Node.js                                             │
│  • Zero npm packages in runtime                              │
│  • Microsecond response times                                │
└─────────────────────────────────────────────────────────────┘
```

## Path Aliases

This project uses `@/` to reference `src/`:

```tsx
import { MyComponent } from "@/components/my-component";
import { utils } from "@/lib/utils";
```

## File Structure

```
my-app/
├── src/
│   ├── App.tsx              # Main component
│   ├── components/
│   │   └── ui/              # UI components
│   └── lib/
│       └── utils.ts         # Utilities
├── albedo.config.ts         # AlBDO configuration
├── tsconfig.json            # TypeScript + JSX support
└── package.json
```
