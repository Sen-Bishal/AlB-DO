import { Button } from "@/components/ui/button";
import { Card, CardHeader, CardTitle, CardContent } from "@/components/ui/card";

export default function App() {
  return (
    <main className="container mx-auto p-8">
      <h1 className="text-4xl font-bold mb-8">AlBDO + Shadcn/UI Demo</h1>

      <section className="mb-8">
        <h2 className="text-2xl font-semibold mb-4">External Components</h2>
        <p className="text-slate-600 mb-4">
          AlBDO optimizes your components while keeping full compatibility with
          external libraries like shadcn/ui, Tailwind CSS, and more.
        </p>
      </section>

      <section className="mb-8">
        <h2 className="text-2xl font-semibold mb-4">Button Variants</h2>
        <div className="flex gap-4">
          <Button>Default Button</Button>
          <Button variant="outline">Outline Button</Button>
          <Button variant="ghost">Ghost Button</Button>
        </div>
      </section>

      <section className="mb-8">
        <h2 className="text-2xl font-semibold mb-4">Card Component</h2>
        <Card className="max-w-md">
          <CardHeader>
            <CardTitle>AlBDO + External Libraries</CardTitle>
          </CardHeader>
          <CardContent>
            <p className="text-slate-600">
              This card is built with your own components. AlBDO analyzes and
              optimizes them for maximum performance.
            </p>
          </CardContent>
        </Card>
      </section>

      <section className="mb-8">
        <h2 className="text-2xl font-semibold mb-4">Third-Party Imports</h2>
        <p className="text-slate-600">
          Import any npm package. AlBDO bundles vendor code efficiently and
          only ships what's needed.
        </p>
      </section>
    </main>
  );
}
