import React from 'react';
import FeatureCard from './FeatureCard';

export default function Features() {
  return (
    <section className="features">
      <FeatureCard title="Fast" />
      <FeatureCard title="Reliable" />
      <FeatureCard title="Scalable" />
    </section>
  );
}