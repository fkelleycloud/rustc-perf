<script setup lang="ts">
const props = defineProps<{
  cases: number;
  benchmarks: number;
}>();
</script>

<template>
  <span :title="cases + ' test case(s), ' + benchmarks + ' unique benchmark(s)'"
    >{{ cases }} ({{ benchmarks }})</span
  >
</template>
