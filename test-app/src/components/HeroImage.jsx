import React from 'react';

export default function HeroImage({ src, alt }) {
  return (
    <div className="hero">
      <img src={src} alt={alt} />
      <h1>Welcome</h1>
    </div>
  );
}