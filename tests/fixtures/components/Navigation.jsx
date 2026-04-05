import React from 'react';
import Button from './Button';

export function Navigation() {
  return (
    <nav>
      <Button>Home</Button>
      <Button>About</Button>
      <Button>Contact</Button>
    </nav>
  );
}

export default Navigation;