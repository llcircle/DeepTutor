import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";

import { ReadingLibraryPage } from "@/components/reading/library/ReadingLibrary";
import { MaterialLibraryPage } from "@/components/reading/library/MaterialLibrary";

const mock = vi.hoisted(() => ({
  listReadingWorkspaces: vi.fn(),
  learningLibrary: vi.fn(),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => ({ push: vi.fn(), replace: vi.fn() }),
}));

vi.mock("next/link", () => ({
  default: ({
    href,
    children,
    ...props
  }: {
    href: string;
    children: React.ReactNode;
    [key: string]: unknown;
  }) => (
    <a href={href} {...props}>
      {children}
    </a>
  ),
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
    i18n: { language: "en" },
  }),
}));

vi.mock("@/components/learning/create-request", () => ({
  useCreateRequest: vi.fn(),
  requestedLearningCreation: false,
}));

vi.mock("@/lib/learning-library", () => ({
  learningLibrary: mock.learningLibrary,
  libraryItemKey: (_row: unknown, id: string) => id,
}));

vi.mock("@/lib/reading-workspace-api", () => ({
  listReadingWorkspaces: mock.listReadingWorkspaces,
  deleteReadingWorkspace: vi.fn(),
  retryReadingMaterial: vi.fn(),
  deleteReadingMaterial: vi.fn(),
  addReadingWorkspaceMaterial: vi.fn(),
}));

vi.mock("@/components/learning/LearningShell", () => ({
  LearningEmptyState: ({ title }: { title: string }) => <p>{title}</p>,
  LearningErrorState: ({ message }: { message: string }) => (
    <p role="alert">{message}</p>
  ),
  LearningSkeleton: () => <p>loading</p>,
}));

vi.mock("@/components/reading/library/LibraryShell", () => ({
  LibraryShell: ({ children }: { children: React.ReactNode }) => (
    <main>{children}</main>
  ),
}));

vi.mock("@/components/reading/library/InvidiousAccountFeedback", () => ({
  default: () => null,
}));

vi.mock("@/components/reading/library/FolderDialog", () => ({
  FolderDialog: () => null,
}));

vi.mock("@/components/reading/library/AddMaterialsDialog", () => ({
  AddMaterialsDialog: () => null,
}));

const collection = {
  workspace_id: "rws_course-notes",
  title: "Course Notes",
  description: "Lecture materials",
  color: "",
  active_material_id: null,
  created_at: 100,
  updated_at: 200,
  tabs: [],
};

beforeEach(() => {
  window.history.replaceState({}, "", "/learning/reading?dt_workspace=");
  mock.listReadingWorkspaces.mockResolvedValue([collection]);
  mock.learningLibrary.mockResolvedValue({ items: [] });
});

it("shows created reading folders in the collections page", async () => {
  render(<ReadingLibraryPage />);

  expect(await screen.findByText("Course Notes")).toBeInTheDocument();
  expect(mock.listReadingWorkspaces).toHaveBeenCalledOnce();
  expect(mock.learningLibrary).toHaveBeenCalledWith("materials");
  expect(mock.learningLibrary).not.toHaveBeenCalledWith("reading");
});

it("loads created folders for material collection membership", async () => {
  render(<MaterialLibraryPage />);

  await waitFor(() =>
    expect(mock.listReadingWorkspaces).toHaveBeenCalledOnce(),
  );
  expect(mock.learningLibrary).toHaveBeenCalledWith("materials");
  expect(mock.learningLibrary).not.toHaveBeenCalledWith("reading");
});
