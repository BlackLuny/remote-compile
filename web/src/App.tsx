import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { BrowserRouter, Navigate, Route, Routes } from "react-router-dom";
import { api, ApiError, type Me } from "./api";
import { Layout } from "./components/Layout";
import { Spinner } from "./components/ui";
import { Login } from "./pages/Login";
import { Overview } from "./pages/Overview";
import { Tasks } from "./pages/Tasks";
import { TaskDetail } from "./pages/TaskDetail";
import { Workers } from "./pages/Workers";
import { WorkerDetail } from "./pages/WorkerDetail";
import { Images } from "./pages/Images";
import { Profiles } from "./pages/Profiles";
import { Storage } from "./pages/Storage";
import { Settings } from "./pages/Settings";

const client = new QueryClient({
  defaultOptions: {
    queries: {
      // Live updates arrive over SSE; polling is the fallback, not the plan.
      refetchOnWindowFocus: false,
      staleTime: 5_000,
      retry: (count, error) =>
        // Never retry an auth failure — that just delays the login screen.
        !(error instanceof ApiError && error.status === 401) && count < 2,
    },
  },
});

function Shell() {
  const me = useQuery({
    queryKey: ["me"],
    queryFn: () => api.get<Me>("/api/me"),
    retry: false,
  });

  if (me.isLoading) return <Spinner label="正在确认会话…" />;
  if (me.isError || !me.data) return <Navigate to="/login" replace />;

  return (
    <Routes>
      <Route element={<Layout me={me.data} />}>
        <Route index element={<Overview />} />
        <Route path="tasks" element={<Tasks />} />
        <Route path="tasks/:id" element={<TaskDetail role={me.data.role} />} />
        <Route path="workers" element={<Workers role={me.data.role} />} />
        <Route path="workers/:id" element={<WorkerDetail role={me.data.role} />} />
        <Route path="images" element={<Images role={me.data.role} />} />
        <Route path="profiles" element={<Profiles role={me.data.role} />} />
        <Route path="storage" element={<Storage role={me.data.role} />} />
        <Route path="settings" element={<Settings role={me.data.role} />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Route>
    </Routes>
  );
}

export default function App() {
  return (
    <QueryClientProvider client={client}>
      <BrowserRouter>
        <Routes>
          <Route path="/login" element={<Login />} />
          <Route path="/*" element={<Shell />} />
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
